//! OpenClaude runtime adapter — v1 [`RuntimeClient`], grounded against
//! OpenClaude 0.15.0.
//!
//! **Decision: public SDK, no fork; transport = NDJSON over stdio to a thin TS bridge.**
//! OpenClaude is Node-side, so a small `bridge.ts` runs the SDK `query()` and this Rust
//! client drives it as a child process: one line in (`invoke`), a stream of `permission`
//! events out (each answered by the Rust [`ActionResponder`] — **the wall**), and a final
//! `result` carrying the structured [`AgentOutput`] (the SDK has no public JSON-schema output,
//! so the bridge instructs the model to make its FINAL message a JSON object and parses it —
//! provider-agnostic, unlike an MCP `submit` tool which perturbed provider routing;
//! live-verified).
//!
//! Why stdio, not gRPC: no `protoc`/`tonic`/ports, and a pipe to a child is *more* confined
//! than a localhost socket (nothing binds, nothing impersonable). The approval channel
//! is the child's stdin/stdout — local by construction.
//!
//! The client is parameterized by its spawn `command`, so it drives the real `bun run
//! bridge.ts` in production and a mock script in tests (same protocol, no network).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::{
    ActionDecision, ActionRequest, ActionResponder, InvocationRequest, RuntimeClient, SessionId,
};
use crate::error::{DackError, Result};
use crate::model::proposal::AgentOutput;
use crate::sandbox::{ExecKind, HostSandbox, IsolationPolicy, Mount, ProcessSpec, Sandbox};

/// Tools we deny at the engine boundary too (defense-in-depth over the wall, which already
/// denies the `Shell` class in every state).
/// Raw shell bypasses path-gating, so it never belongs in a duck state.
const ALWAYS_DISALLOWED: &[&str] = &["Bash", "PowerShell", "REPL", "KillShell"];

/// The SDK-level `disallowedTools` for an invocation, derived from the running spec.
///
/// The duck NEVER gets raw shell/exec: `Shell` is in no consciousness state's `tool_scope`, and we
/// disallow these at the SDK layer too (defense-in-depth on top of the wall). A **worker**, though,
/// is a sandboxed build agent whose spec DOES admit `Shell` — it must keep `Bash`. Its shell is
/// bounded by the wall (`worker_spec` + the temp-workspace relativize root) and the throwaway
/// workspace, not by stripping the tool here. Stripping it was a real bug: with `Bash` absent from
/// the engine's tool registry, (1) `injectAgents` threw on any agent referencing `Bash` (aborting
/// sub-helper injection) and (2) the worker model, having no native `Bash`, emitted text-format
/// `<function=Bash>` tool calls that executed nothing — so the worker built nothing.
fn disallowed_for(spec: &crate::state::StateSpec) -> Vec<String> {
    let worker_keeps_bash = spec.tool_scope.allows(crate::state::ToolClass::Shell);
    let mut out: Vec<String> = ALWAYS_DISALLOWED
        .iter()
        .filter(|t| !(worker_keeps_bash && **t == "Bash"))
        .map(|s| s.to_string())
        .collect();
    // Defense-in-depth: NETWORK egress (WebFetch/WebSearch = `Net`) is a distinct class scoped to
    // Perceive (+ the worker). Where a state's scope does NOT admit `Net`, hide these at the SDK layer
    // too — so an Express/Settle/Reflect cycle isn't even offered the network (the wall denies it
    // regardless, but not offering it stops an injected model from trying / burning a turn).
    if !spec.tool_scope.allows(crate::state::ToolClass::Net) {
        out.push("WebFetch".into());
        out.push("WebSearch".into());
    }
    out
}

/// Rust → bridge.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ToBridge<'a> {
    Invoke {
        system_prompt: &'a str,
        user_prompt: String,
        disallowed_tools: Vec<String>,
        allowed_tools: Option<Vec<String>>,
        model: Option<&'a str>,
        /// The agent's working dir (the soul repo) → the bridge's `options.cwd`.
        cwd: Option<&'a str>,
        /// Sticky-session resume: an engine `session_id` to continue (`options.resume`). `None` =
        /// a fresh session (the firebreak default; set only for sticky state-prompts).
        resume: Option<&'a str>,
        /// Resolved MCP capability servers (SDK-shaped, tokens injected) → `options.mcpServers`.
        mcp_servers: &'a std::collections::BTreeMap<String, serde_json::Value>,
        /// Sub-agent defs the engine may `Task`-spawn → `options.agents`. Empty for the
        /// duck's states; a worker registers its sub-helpers here.
        agents: &'a std::collections::BTreeMap<String, serde_json::Value>,
    },
    Decision {
        tool_use_id: String,
        allow: bool,
        message: Option<String>,
    },
}

/// bridge → Rust.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FromBridge {
    /// A `canUseTool` event: answer via the wall, then reply with a `Decision`.
    Permission {
        tool: String,
        tool_use_id: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    /// The run finished; `output` is the agent's `submit`ted structured proposal. `session_id` is
    /// the engine session this run ran in (for sticky-session resume), when the bridge reports it.
    /// `usage` is the SDK's token accounting for this turn — its `input_tokens` + `cache_read_input_tokens`
    /// = the resumed CONTEXT size, which the harness uses to size-evict a bloated sticky session.
    Result {
        output: AgentOutput,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        usage: Option<super::InvokeUsage>,
    },
    /// The run failed inside the engine/bridge.
    Error { message: String },
    /// Diagnostic passthrough (surfaced to the harness log).
    Log { message: String },
}

pub struct OpenClaudeClient {
    /// How to spawn the bridge, e.g. `["bun", "run", "bridge.ts"]`. In tests, a mock script.
    pub command: Vec<String>,
    /// Working dir for the spawn (the `openclaude-bridge/` project in production).
    pub cwd: Option<PathBuf>,
    /// Env **overlaid** on the inherited process env for the bridge (provider key/base-URL +
    /// `forwarded_env`). We inherit rather than clear because the engine's auth context can
    /// live in ambient env the SDK requires; the soul key is never in env regardless
    /// (it is a `file://` secret ref).
    pub env: HashMap<String, String>,
    /// Model id passed to the SDK (e.g. `mimo-v2.5-pro`); `None` = bridge/provider default.
    pub model: Option<String>,
    /// The **connector's model channel** (set from `config.runtime.connector`): `true` routes the
    /// per-invocation model via the child's `OPENAI_MODEL` env (an OpenAI-compatible gateway, which
    /// rejects a gateway name as `options.model`); `false` routes it via `options.model` (the
    /// Anthropic catalog). Replaces the former `CLAUDE_CODE_USE_OPENAI` env-sniff.
    pub model_via_env: bool,
    /// Isolation backend for the bridge process (default [`HostSandbox`]). Under a container
    /// backend the **soul repo (`workdir`) is mounted as a writable volume** so the agent's
    /// memory/skill writes land while the rest of the box stays out of reach.
    pub sandbox: Arc<dyn Sandbox>,
    pub policy: IsolationPolicy,
    /// **Worker isolation backend**. `None` ⇒ delegated workers run on the host (today's
    /// behavior). `Some` ⇒ a `req.isolate` invocation runs the bridge in this backend (Docker),
    /// guest-relative. The duck (`isolate:false`) NEVER touches it. Built from `runtime.worker_sandbox`.
    pub worker: Option<WorkerBackend>,
    /// Wall-clock budget for one whole invocation (incl. the wall round-trips). A hung LLM/bridge
    /// would otherwise freeze the single-flight loop forever; on elapse `invoke` returns a
    /// `Runtime` error (dispatch logs + continues) and `kill_on_drop` reaps the child.
    pub timeout: Duration,
}

/// The container backend for delegated workers: the [`Sandbox`] (Docker), the
/// [`IsolationPolicy`] (image/net/caps), the in-IMAGE bridge `command`, and the guest `cwd`
/// (`/workspace`) the workspace is bind-mounted at. Cloned per-invocation; cheap (Arcs + small vecs).
#[derive(Clone)]
pub struct WorkerBackend {
    pub sandbox: Arc<dyn Sandbox>,
    pub policy: IsolationPolicy,
    /// In-image bridge invocation, e.g. `["bun","run","/app/openclaude-bridge/bridge.ts"]`.
    pub command: Vec<String>,
    /// Guest working dir the workspace mounts at and the bridge chdir's to, e.g. `/workspace`.
    pub cwd: PathBuf,
}

impl OpenClaudeClient {
    /// The production bridge: `bun run bridge.ts` inside the `openclaude-bridge/` project.
    pub fn bun_bridge(
        bridge_dir: impl Into<PathBuf>,
        env: HashMap<String, String>,
        model: Option<String>,
        model_via_env: bool,
        timeout: Duration,
    ) -> Self {
        Self {
            command: vec!["bun".into(), "run".into(), "bridge.ts".into()],
            cwd: Some(bridge_dir.into()),
            env,
            model,
            model_via_env,
            sandbox: Arc::new(HostSandbox),
            policy: IsolationPolicy::host_passthrough(),
            worker: None, // host workers by default; the CLI sets a Docker backend when configured
            timeout,
        }
    }

    /// Attach (or clear) the worker isolation backend — the CLI builds it from `runtime.worker_sandbox`.
    pub fn with_worker(mut self, worker: Option<WorkerBackend>) -> Self {
        self.worker = worker;
        self
    }

    /// The NARROWED env a containerized worker receives — ONLY the model-channel creds/flags from the
    /// client env (gateway or Anthropic). The duck's ambient/forwarded env and host `PATH`/`HOME` are
    /// deliberately excluded (the container has its own toolchain + PATH); `OPENAI_MODEL` and
    /// `DACK_BRIDGE_CWD` are added by `invoke`. Least privilege: the worker can call the model, nothing more.
    fn worker_env(&self) -> std::collections::BTreeMap<String, String> {
        const KEYS: &[&str] = &[
            "OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_API_BASE", "CLAUDE_CODE_USE_OPENAI",
            "ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN",
        ];
        self.env
            .iter()
            .filter(|(k, _)| KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Render context blocks into one user turn with **visible trust framing**:
    /// the trusted directive and the untrusted world data are fenced and labelled so the
    /// model (and any reviewer) can see which is which. Framing lives here, in the harness.
    fn render_blocks(blocks: &[super::ContextBlock]) -> String {
        blocks
            .iter()
            .map(|b| {
                let tag = if b.trusted {
                    "TRUSTED-DIRECTIVE"
                } else {
                    "UNTRUSTED-WORLD-DATA"
                };
                format!("<{tag} label=\"{}\">\n{}\n</{tag}>", b.label, b.body)
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Route the effective per-invocation model to the channel the engine reads, returning
/// `(openai_model_env, options_model)`. On an **OpenAI-compatible gateway** (`CLAUDE_CODE_USE_OPENAI`
/// set) the SDK takes the model from the `OPENAI_MODEL` env and REJECTS a gateway name passed as
/// `options.model` against its Anthropic catalog — so the model goes to `OPENAI_MODEL` and
/// `options.model` stays `None`. On the **Anthropic catalog** path it's the reverse. The effective
/// model is the per-invocation override (8.7) if present, else the client's configured default.
/// (Pure + total so the routing is unit-testable without spawning the bridge.)
fn resolve_model_env(
    openai_mode: bool,
    req_model: Option<&str>,
    self_model: Option<&str>,
) -> (Option<String>, Option<String>) {
    let effective = req_model.or(self_model).map(str::to_string);
    if openai_mode {
        (effective, None)
    } else {
        (None, effective)
    }
}

#[async_trait]
impl RuntimeClient for OpenClaudeClient {
    fn worker_guest_cwd(&self) -> Option<PathBuf> {
        self.worker.as_ref().map(|w| w.cwd.clone())
    }

    async fn invoke(
        &self,
        req: InvocationRequest,
        responder: Arc<dyn ActionResponder>,
    ) -> Result<(AgentOutput, Option<SessionId>, Option<super::InvokeUsage>)> {
        // Per-invocation model (8.7) routed to the channel the engine reads (see [`resolve_model_env`]):
        // on an OpenAI-compatible gateway the model goes to the child's `OPENAI_MODEL` env (the SDK
        // rejects a gateway name as `options.model`); on the Anthropic catalog it goes to `options.model`.
        let (openai_model_env, options_model) =
            resolve_model_env(self.model_via_env, req.model.as_deref(), self.model.as_deref());

        // a delegated worker (`req.isolate`) with a configured backend runs the bridge in a
        // CONTAINER, everything GUEST-relative — the workspace is bind-mounted at the guest cwd,
        // `DACK_BRIDGE_CWD` IS that guest cwd, and the caller set the wall's repo_root to the SAME guest
        // path (the SDK inside the container emits container-absolute paths; the three must agree or
        // writes fail to relativize → denied). Env is NARROWED to the model channel. The duck
        // (`isolate:false`) takes the host path below — byte-for-byte today's behavior (workdir mounted
        // rw, the harness env inherited; the bridge chdir's to `DACK_BRIDGE_CWD` before loading the SDK).
        let worker = if req.isolate { self.worker.as_ref() } else { None };
        let (sandbox, spec): (&Arc<dyn Sandbox>, ProcessSpec) = match worker {
            Some(w) => {
                let host_ws = req.workdir.clone().unwrap_or_else(|| PathBuf::from("."));
                let guest = w.cwd.clone();
                let mut env = self.worker_env();
                if let Some(m) = &openai_model_env {
                    env.insert("OPENAI_MODEL".to_string(), m.clone());
                }
                env.insert("DACK_BRIDGE_CWD".to_string(), guest.to_string_lossy().into_owned());
                // The SDK writes its config (`~/.openclaude.json`) at init — but the container rootfs
                // is `--read-only`, so point `HOME` at the writable `/tmp` tmpfs (else EROFS on /root).
                env.insert("HOME".to_string(), "/tmp".to_string());
                // Tell the SDK it is ALREADY in a sandbox so it does NOT enable its own (Docker-based)
                // bash sandbox — which can't run inside this container (no docker-in-docker) and dies
                // with "failed to connect to the docker API". DACK's container IS the sandbox.
                env.insert("IS_SANDBOX".to_string(), "1".to_string());
                env.extend(req.secret_env.clone()); // empty for a worker today, honored if ever set
                let mut mounts =
                    vec![Mount { host: host_ws.clone(), guest: guest.clone(), writable: true }];
                mounts.extend(req.mounts.iter().cloned()); // the agent's read-only `volumes:`
                // Container name = the workspace dir basename (a unique run-id) so the harness can
                // reap it by name (`kill_on_drop` only kills the local `docker` client).
                let name = host_ws.file_name().map(|n| n.to_string_lossy().into_owned());
                let spec = ProcessSpec {
                    command: w.command.clone(),
                    cwd: guest,
                    env,
                    clear_env: false,
                    kind: ExecKind::Worker,
                    mounts,
                    policy: w.policy.clone(),
                    name,
                };
                (&w.sandbox, spec)
            }
            None => {
                let mut env: std::collections::BTreeMap<String, String> =
                    self.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                env.extend(req.secret_env.clone());
                if let Some(m) = &openai_model_env {
                    env.insert("OPENAI_MODEL".to_string(), m.clone());
                }
                if let Some(w) = req.workdir.as_ref().and_then(|p| p.to_str()) {
                    env.insert("DACK_BRIDGE_CWD".to_string(), w.to_string());
                }
                let spec = ProcessSpec {
                    command: self.command.clone(),
                    cwd: self.cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
                    env,
                    clear_env: false,
                    kind: ExecKind::Agent,
                    mounts: req
                        .workdir
                        .as_ref()
                        .map(|w| vec![Mount { host: w.clone(), guest: w.clone(), writable: true }])
                        .unwrap_or_default(),
                    policy: self.policy.clone(),
                    name: None,
                };
                (&self.sandbox, spec)
            }
        };
        let mut child = sandbox
            .command(&spec)?
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| DackError::Runtime(format!("spawn bridge: {e}")))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| DackError::Runtime("bridge stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DackError::Runtime("bridge stdout unavailable".into()))?;
        let mut lines = BufReader::new(stdout).lines();

        // Send the one invocation. stdin stays open for decision replies.
        let invoke = ToBridge::Invoke {
            system_prompt: &req.system_prompt,
            user_prompt: Self::render_blocks(&req.blocks),
            disallowed_tools: disallowed_for(&req.spec),
            allowed_tools: req.allowed_tools.clone(),
            // Catalog path only — None on the gateway (the model went via OPENAI_MODEL above).
            model: options_model.as_deref(),
            cwd: req.workdir.as_deref().and_then(|p| p.to_str()),
            // Sticky-session resume: the engine session id the harness wants to continue (if any).
            resume: req.session.as_ref().map(|s| s.0.as_str()),
            mcp_servers: &req.mcp_servers,
            agents: &req.agents,
        };

        // The whole exchange (send → permission round-trips → result) runs under ONE wall-clock
        // budget. A hung LLM/bridge turn elapses here instead of freezing the single-flight loop
        // forever; on return the child is reaped by `kill_on_drop`.
        let drive = async {
            write_line(&mut stdin, &invoke).await?;
            loop {
                let line = lines
                    .next_line()
                    .await
                    .map_err(|e| DackError::Runtime(format!("bridge read: {e}")))?;
                let Some(line) = line else {
                    return Err(DackError::Runtime("bridge closed before result".into()));
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<FromBridge>(&line).map_err(|e| {
                    DackError::Runtime(format!("bridge event parse: {e} (line: {line})"))
                })? {
                    FromBridge::Permission {
                        tool,
                        tool_use_id,
                        input,
                    } => {
                        let decision = responder
                            .decide(&ActionRequest {
                                tool,
                                tool_use_id: tool_use_id.clone(),
                                input,
                            })
                            .await;
                        let (allow, message) = match decision {
                            ActionDecision::Allow => (true, None),
                            ActionDecision::Deny(why) => (false, Some(why)),
                        };
                        write_line(
                            &mut stdin,
                            &ToBridge::Decision {
                                tool_use_id,
                                allow,
                                message,
                            },
                        )
                        .await?;
                    }
                    FromBridge::Result { output, session_id, usage } => {
                        return Ok((output, session_id.map(SessionId), usage))
                    }
                    FromBridge::Error { message } => return Err(DackError::Runtime(message)),
                    FromBridge::Log { message } => tracing::info!("[bridge] {message}"),
                }
            }
        };

        // Per-invocation override (e.g. Reflect's longer budget) wins over the client default.
        let budget = req.timeout.unwrap_or(self.timeout);
        match tokio::time::timeout(budget, drive).await {
            Ok(result) => result,
            Err(_elapsed) => Err(DackError::Timeout(format!(
                "invoke exceeded its {budget:?} budget (LLM/bridge hung)"
            ))),
        }
    }
}

async fn write_line<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &ToBridge<'_>) -> Result<()> {
    let mut buf = serde_json::to_vec(msg)?;
    buf.push(b'\n');
    w.write_all(&buf)
        .await
        .map_err(|e| DackError::Runtime(format!("bridge write: {e}")))?;
    w.flush()
        .await
        .map_err(|e| DackError::Runtime(format!("bridge flush: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ContextBlock;
    use crate::state::{default_spec, worker_spec, ConsciousnessState};
    use std::sync::Mutex;

    /// The duck never gets raw shell (Bash stays disallowed at the SDK layer); a WORKER (whose spec
    /// admits `Shell`) keeps Bash so its native tool calls actually execute. Other exec tools stay off.
    #[test]
    fn disallowed_tools_strips_bash_for_duck_keeps_it_for_worker() {
        let duck = disallowed_for(&default_spec(ConsciousnessState::Express));
        assert!(duck.contains(&"Bash".to_string()), "the duck must NOT get Bash");

        let worker = disallowed_for(&worker_spec());
        assert!(!worker.contains(&"Bash".to_string()), "a worker MUST keep Bash");
        // The other exec tools remain disallowed even for a worker (it builds with Bash, not these).
        assert!(worker.contains(&"PowerShell".to_string()));
        assert!(worker.contains(&"KillShell".to_string()));
    }

    /// The model-routing split (8.7 + the gateway fix): on a gateway the resolved model must go to
    /// the `OPENAI_MODEL` env (NOT `options.model`, which the SDK rejects); on the catalog, reverse.
    #[test]
    fn resolve_model_env_routes_by_engine() {
        // Gateway: per-invocation override → OPENAI_MODEL; options.model stays None.
        assert_eq!(
            resolve_model_env(true, Some("xiaomi/mimo-v2.5-pro"), Some("nvidia/nemotron:free")),
            (Some("xiaomi/mimo-v2.5-pro".into()), None)
        );
        // Gateway, no per-state override → the client default goes to OPENAI_MODEL.
        assert_eq!(
            resolve_model_env(true, None, Some("nvidia/nemotron:free")),
            (Some("nvidia/nemotron:free".into()), None)
        );
        // Catalog (Anthropic): the model goes to options.model; OPENAI_MODEL untouched.
        assert_eq!(
            resolve_model_env(false, Some("claude-haiku-4-5"), None),
            (None, Some("claude-haiku-4-5".into()))
        );
        // Nothing configured → nothing on either channel (the engine's own default stands).
        assert_eq!(resolve_model_env(true, None, None), (None, None));
    }

    /// A recording responder: captures every tool it was asked about, returns a fixed verdict.
    struct Recorder {
        asked: Mutex<Vec<String>>,
        allow: bool,
    }
    #[async_trait]
    impl ActionResponder for Recorder {
        async fn decide(&self, req: &ActionRequest) -> ActionDecision {
            self.asked.lock().unwrap().push(req.tool.clone());
            if self.allow {
                ActionDecision::Allow
            } else {
                ActionDecision::Deny("test deny".into())
            }
        }
    }

    /// Write a `/bin/sh` mock bridge and return a client that spawns it.
    fn mock_client(script: &str, tag: &str) -> OpenClaudeClient {
        let dir = std::env::temp_dir().join(format!("dack-bridge-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mock.sh");
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut env = HashMap::new();
        if let Ok(p) = std::env::var("PATH") {
            env.insert("PATH".into(), p);
        }
        OpenClaudeClient {
            command: vec!["/bin/sh".into(), path.to_string_lossy().into()],
            cwd: None,
            env,
            model: None,
            model_via_env: false,
            sandbox: Arc::new(HostSandbox),
            policy: IsolationPolicy::host_passthrough(),
            worker: None,
            timeout: Duration::from_secs(30),
        }
    }

    fn perceive_req() -> InvocationRequest {
        InvocationRequest {
            spec: default_spec(ConsciousnessState::Perceive),
            system_prompt: "SOUL + perceive".into(),
            blocks: vec![ContextBlock {
                label: "world".into(),
                body: "a tweet".into(),
                trusted: false,
            }],
            session: None,
            workdir: None,
            secret_env: Default::default(),
            mcp_servers: Default::default(),
            model: None,
            agents: Default::default(),
            isolate: false,
            mounts: Vec::new(),
            allowed_tools: None,
            timeout: None,
        }
    }

    #[tokio::test]
    async fn relays_permission_to_the_wall_then_parses_result() {
        // The mock induces one permission event, blocks until the decision arrives
        // (proving the round-trip), then returns a structured result.
        let mock = "#!/bin/sh\n\
            read invoke\n\
            printf '{\"kind\":\"permission\",\"tool\":\"Write\",\"tool_use_id\":\"tu_1\",\"input\":{\"file_path\":\"/x/p.txt\"}}\\n'\n\
            read decision\n\
            printf '{\"kind\":\"result\",\"output\":{\"thought\":\"done\",\"transition\":{\"to_prompt\":null}}}\\n'\n";
        let client = mock_client(mock, "perm");
        let rec = Arc::new(Recorder {
            asked: Mutex::new(vec![]),
            allow: false,
        });
        let (out, _, _) = client.invoke(perceive_req(), rec.clone()).await.unwrap();

        assert_eq!(out.thought, "done");
        assert!(out.transition.to_prompt.is_none());
        // The wall WAS consulted with the real tool; the mock only reached `result`
        // because the decision line was delivered (it blocks on `read decision`).
        assert_eq!(*rec.asked.lock().unwrap(), vec!["Write".to_string()]);
    }

    #[tokio::test]
    async fn result_only_run_needs_no_decisions() {
        let mock = "#!/bin/sh\n\
            read invoke\n\
            printf '{\"kind\":\"result\",\"output\":{\"thought\":\"hi\",\"proposal\":{\"intent\":\"noop\",\"gist\":\"g\"},\"transition\":{\"to_state\":null}}}\\n'\n";
        let client = mock_client(mock, "resultonly");
        let rec = Arc::new(Recorder {
            asked: Mutex::new(vec![]),
            allow: true,
        });
        let (out, _, _) = client.invoke(perceive_req(), rec).await.unwrap();
        assert_eq!(out.thought, "hi");
        assert_eq!(out.proposal.unwrap().gist, "g");
    }

    // ── Docker-isolated workers ────────────────────────────────────────────────

    /// Records the [`ProcessSpec`] it's handed, then runs a fixed mock bridge so the NDJSON protocol
    /// completes — lets us assert the spec the runtime builds WITHOUT a real Docker daemon (it ignores
    /// `spec.command`, which would be the in-image `bun run …`).
    struct RecordingSandbox {
        last: Mutex<Option<ProcessSpec>>,
        mock: PathBuf,
    }
    impl Sandbox for RecordingSandbox {
        fn command(&self, spec: &ProcessSpec) -> Result<tokio::process::Command> {
            *self.last.lock().unwrap() = Some(spec.clone());
            let mut c = tokio::process::Command::new("/bin/sh");
            c.arg(&self.mock);
            Ok(c)
        }
    }

    const RESULT_MOCK: &str = "#!/bin/sh\nread invoke\n\
        printf '{\"kind\":\"result\",\"output\":{\"thought\":\"ok\",\"transition\":{\"to_prompt\":null}}}\\n'\n";

    /// A client whose sandbox (and, optionally, worker backend) is a [`RecordingSandbox`]. The env
    /// carries a gateway cred (must cross to a worker) + `DACK_HANDLE` (a duck-ambient var that must NOT).
    fn recording_client(tag: &str, with_worker: bool) -> (OpenClaudeClient, Arc<RecordingSandbox>) {
        let dir = std::env::temp_dir().join(format!("dack-recsb-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mock.sh");
        std::fs::write(&path, RESULT_MOCK).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let rec = Arc::new(RecordingSandbox { last: Mutex::new(None), mock: path });
        let mut env = HashMap::new();
        env.insert("OPENAI_API_KEY".into(), "sk-test".into());
        env.insert("CLAUDE_CODE_USE_OPENAI".into(), "1".into());
        env.insert("DACK_HANDLE".into(), "duck".into()); // ambient — must NOT reach the worker
        let worker = with_worker.then(|| WorkerBackend {
            sandbox: rec.clone(),
            policy: IsolationPolicy::locked_down("dack/worker:test"),
            command: vec!["bun".into(), "run".into(), "/app/openclaude-bridge/bridge.ts".into()],
            cwd: PathBuf::from("/workspace"),
        });
        let client = OpenClaudeClient {
            command: vec!["bun".into(), "run".into(), "bridge.ts".into()],
            cwd: Some(PathBuf::from("/bridge")),
            env,
            model: None,
            model_via_env: true,
            sandbox: rec.clone(),
            policy: IsolationPolicy::host_passthrough(),
            worker,
            timeout: Duration::from_secs(30),
        };
        (client, rec)
    }

    fn worker_req(workdir: &str, isolate: bool, mounts: Vec<Mount>) -> InvocationRequest {
        InvocationRequest {
            spec: crate::state::worker_spec(),
            system_prompt: "agent prompt".into(),
            blocks: vec![],
            session: None,
            workdir: Some(PathBuf::from(workdir)),
            secret_env: Default::default(),
            mcp_servers: Default::default(),
            model: None,
            agents: Default::default(),
            isolate,
            mounts,
            allowed_tools: None,
            timeout: None,
        }
    }

    /// The crux: an isolated worker runs from the in-image command at the GUEST `/workspace`, with the
    /// workspace bind-mounted rw + the agent's ro volumes, a reap-`--name`, and a NARROWED env.
    #[tokio::test]
    async fn worker_invoke_builds_guest_spec_with_narrowed_env() {
        let (client, rec) = recording_client("wspec", true);
        let mounts = vec![Mount {
            host: PathBuf::from("/soul/memory"),
            guest: PathBuf::from("/mnt/memory"),
            writable: false,
        }];
        let req = worker_req("/soul/workspaces/dack-worker-coder-1-0", true, mounts);
        client
            .invoke(req, Arc::new(Recorder { asked: Mutex::new(vec![]), allow: true }))
            .await
            .unwrap();
        let spec = rec.last.lock().unwrap().clone().unwrap();
        assert_eq!(spec.kind, ExecKind::Worker);
        assert_eq!(spec.command, vec!["bun", "run", "/app/openclaude-bridge/bridge.ts"]);
        assert_eq!(spec.cwd, PathBuf::from("/workspace"));
        assert_eq!(spec.policy.image, "dack/worker:test");
        // mount[0] = workspace rw at the guest cwd; mount[1] = the agent's ro volume.
        assert_eq!(spec.mounts[0].host, PathBuf::from("/soul/workspaces/dack-worker-coder-1-0"));
        assert_eq!(spec.mounts[0].guest, PathBuf::from("/workspace"));
        assert!(spec.mounts[0].writable);
        assert_eq!(spec.mounts[1].guest, PathBuf::from("/mnt/memory"));
        assert!(!spec.mounts[1].writable);
        // Reap name = the workspace basename.
        assert_eq!(spec.name.as_deref(), Some("dack-worker-coder-1-0"));
        // GUEST-path triple: DACK_BRIDGE_CWD == the guest cwd (or writes wouldn't relativize).
        assert_eq!(spec.env.get("DACK_BRIDGE_CWD").map(String::as_str), Some("/workspace"));
        // HOME → the writable /tmp tmpfs (the SDK writes ~/.openclaude.json; rootfs is --read-only).
        assert_eq!(spec.env.get("HOME").map(String::as_str), Some("/tmp"));
        // Narrowed env: the gateway cred crosses; the duck's ambient env does NOT.
        assert_eq!(spec.env.get("OPENAI_API_KEY").map(String::as_str), Some("sk-test"));
        assert!(!spec.env.contains_key("DACK_HANDLE"), "ambient duck env must not enter the container");
    }

    /// Back-compat guard: `isolate:false` (the duck) takes the host path — host command, host==guest
    /// mount, the FULL ambient env, no container name. Unchanged from before .
    #[tokio::test]
    async fn host_invoke_keeps_full_env_and_host_mount() {
        let (client, rec) = recording_client("hspec", true); // worker present, but isolate:false
        let req = worker_req("/soul", false, vec![]);
        client
            .invoke(req, Arc::new(Recorder { asked: Mutex::new(vec![]), allow: true }))
            .await
            .unwrap();
        let spec = rec.last.lock().unwrap().clone().unwrap();
        assert_eq!(spec.kind, ExecKind::Agent);
        assert_eq!(spec.command, vec!["bun", "run", "bridge.ts"]); // host command
        assert_eq!(spec.mounts[0].host, spec.mounts[0].guest); // host==guest on the host path
        assert!(spec.name.is_none());
        assert!(spec.env.contains_key("DACK_HANDLE"), "host path inherits the full ambient env");
    }

    /// `isolate:true` but no worker backend configured ⇒ host fallback (the CLI preflight already
    /// hard-failed the boot if `require`-d Docker was missing; here the operator chose host fallback).
    #[tokio::test]
    async fn isolate_without_worker_backend_falls_back_to_host() {
        let (client, rec) = recording_client("fb", false); // worker: None
        let req = worker_req("/soul/ws", true, vec![]);
        client
            .invoke(req, Arc::new(Recorder { asked: Mutex::new(vec![]), allow: true }))
            .await
            .unwrap();
        let spec = rec.last.lock().unwrap().clone().unwrap();
        assert_eq!(spec.kind, ExecKind::Agent, "no backend → host path");
        assert!(spec.name.is_none());
    }

    #[tokio::test]
    async fn invoke_times_out_on_a_hung_bridge() {
        // The mock reads the invoke, then hangs (never emits a result) — the budget must fire.
        let mock = "#!/bin/sh\n\
            read invoke\n\
            sleep 30\n";
        let mut client = mock_client(mock, "hang");
        client.timeout = Duration::from_millis(250); // tight budget for the test
        let rec = Arc::new(Recorder { asked: Mutex::new(vec![]), allow: true });
        let err = client.invoke(perceive_req(), rec).await.unwrap_err();
        assert!(
            err.is_timeout() && matches!(err, DackError::Timeout(m) if m.contains("budget")),
            "a hung bridge must time out (DackError::Timeout, retryable), not freeze the loop"
        );
    }

    #[tokio::test]
    async fn per_invocation_timeout_override_beats_the_client_default() {
        // A hang, a GENEROUS client default, but a TIGHT per-call override (how Reflect would get a
        // LONGER budget, exercised here in reverse: a shorter override must win and fire fast).
        let mock = "#!/bin/sh\n\
            read invoke\n\
            sleep 30\n";
        let mut client = mock_client(mock, "override");
        client.timeout = Duration::from_secs(30);
        let mut req = perceive_req();
        req.timeout = Some(Duration::from_millis(200));
        let rec = Arc::new(Recorder { asked: Mutex::new(vec![]), allow: true });
        let start = std::time::Instant::now();
        let err = client.invoke(req, rec).await.unwrap_err();
        assert!(err.is_timeout(), "the per-invocation override must fire");
        assert!(start.elapsed() < Duration::from_secs(5), "timed out at the 200ms override, not the 30s default");
    }

    #[tokio::test]
    async fn bridge_error_becomes_runtime_error() {
        let mock = "#!/bin/sh\n\
            read invoke\n\
            printf '{\"kind\":\"error\",\"message\":\"provider 401\"}\\n'\n";
        let client = mock_client(mock, "err");
        let rec = Arc::new(Recorder {
            asked: Mutex::new(vec![]),
            allow: true,
        });
        let err = client.invoke(perceive_req(), rec).await.unwrap_err();
        assert!(matches!(err, DackError::Runtime(m) if m.contains("provider 401")));
    }

    #[test]
    fn render_frames_trusted_and_untrusted_blocks() {
        let s = OpenClaudeClient::render_blocks(&[
            ContextBlock { label: "dir".into(), body: "do x".into(), trusted: true },
            ContextBlock { label: "world".into(), body: "evil".into(), trusted: false },
        ]);
        assert!(s.contains("<TRUSTED-DIRECTIVE label=\"dir\">"));
        assert!(s.contains("<UNTRUSTED-WORLD-DATA label=\"world\">"));
    }
}
