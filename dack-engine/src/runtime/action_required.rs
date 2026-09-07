//! The `action_required` responder — **the wall, in code the agent can't touch**
//! Out-of-process, deterministic, dumb. Encodes per-state tool
//! scoping + write-gating, and (future) the Settle predicate.
//!
//! The checks, in order:
//!   1. Is this tool's class in the **current state's allowed set**? (per-state scoping)
//!   2. For file-writes: is `target_path` under one of the state's `writable_dirs`?
//!      And NEVER `runlogs/` — the harness authors the record.
//!   3. Settle needs NO extra predicate: a `tier: settle` tool only ever runs IN Settle, which is
//!      reachable only by an uncontaminated cycle (the taint model). The old `allow_settle`
//!      whitelist+operator_signed reflex was removed — irreversibility is bounded by taint-
//!      reachability here + the custodial/wallet limits externally.
//!
//! Net effect: Perceive auto-denies all write/network-write; Express allows post/reply +
//! memory-append; Settle allows its registered settle-tier tools; everything else denied.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;

use super::classify::classify_tool;
use super::{ActionDecision, ActionRequest, ActionResponder};
use crate::config::CapabilityPrefix;
use crate::state::{StateSpec, ToolClass};

/// One responder per invocation, bound to the running state's spec. Holding the spec
/// (not the state name) means the gate reads the exact same table the rest of the
/// harness does — there is no second, drifting copy of the policy.
pub struct StatePolicyResponder {
    pub spec: StateSpec,
    /// MCP capability prefixes treated as reversible capabilities (→ [`ToolClass::Post`]).
    pub post_tools: Vec<CapabilityPrefix>,
    /// MCP capability prefixes treated as irreversible authority (→ [`ToolClass::SettleTx`]).
    pub settle_tools: Vec<CapabilityPrefix>,
    /// MCP capability prefixes treated as monitoring/read capabilities (→ [`ToolClass::Read`]):
    /// registered `tier: read` servers (e.g. `mcp__cove-read__`). Safe in every state. Each may
    /// carry a `tools` allowlist the classifier enforces fail-closed.
    pub read_tools: Vec<CapabilityPrefix>,
    /// The soul-repo root, for relativizing the **absolute** `file_path`s the SDK emits
    /// before the `writable_dirs` prefix check. `None` in unit tests, which use
    /// repo-relative paths directly.
    pub repo_root: Option<PathBuf>,
    /// DRY-RUN block: tool-name PREFIXES denied here (testing). The agent composes the action — it's
    /// recorded — but the wall stops it executing. Tool-level so a no-trade run still allows
    /// `simulate_swap`/`get_*` while blocking `buy_token`. Empty ⇒ no dry-run blocking (live).
    pub dry_run_block: Vec<String>,
    /// May this cycle write LONG-TERM memory (`memory/`)? Set by the harness = cycle trust ≥
    /// `memory_write_min_trust` (org+). When `false`, a `memory/` write is denied (an untrusted in-run
    /// cycle leaves tags; the digest consolidates). `true` by default (tests / no restriction).
    pub long_term_writable: bool,
    /// `(tool, input)` fingerprints of OUTWARD actions (Post / SettleTx) that ran this invocation —
    /// tracked so [`acted_outward`](Self::acted_outward) can gate the timeout-retry path (never re-run
    /// a wake that already posted/traded → no double-post on a re-run). Duplicates are NOT blocked: a
    /// verbatim repeat is usually a legitimate retry after a send glitch (the wall allowing a call is
    /// not the send succeeding), and irreversibility is bounded EXTERNALLY (chain nonce + provider
    /// limits), not by the wall. Per-invocation; interior-mutable because `decide` takes `&self`.
    outward_seen: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl StatePolicyResponder {
    /// A responder with no capability tools configured (reads/writes only). Used where
    /// the duty exposes no post/settle MCP tools.
    pub fn new(spec: StateSpec) -> Self {
        Self {
            spec,
            post_tools: Vec::new(),
            settle_tools: Vec::new(),
            read_tools: Vec::new(),
            repo_root: None,
            dry_run_block: Vec::new(),
            long_term_writable: true,
            outward_seen: Default::default(),
        }
    }

    /// A responder aware of the deployment's capability MCP tools (operator config).
    pub fn with_capabilities(
        spec: StateSpec,
        post_tools: Vec<CapabilityPrefix>,
        settle_tools: Vec<CapabilityPrefix>,
    ) -> Self {
        Self {
            spec,
            post_tools,
            settle_tools,
            read_tools: Vec::new(),
            repo_root: None,
            dry_run_block: Vec::new(),
            long_term_writable: true,
            outward_seen: Default::default(),
        }
    }

    /// Bind the soul-repo root so live (absolute) write targets are relativized against it
    /// before the `writable_dirs` check. The harness sets this to the canonical soul-repo path.
    pub fn with_repo_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.repo_root = Some(root.into());
        self
    }

    /// Did this invocation ALLOW any OUTWARD action (Post / SettleTx)? Reads the dedup set, which is
    /// populated only when an outward action passes every gate (scope + dry-run) and executes — so a
    /// dry-run-blocked or denied action does NOT count. The dispatch loop uses this to refuse to retry
    /// a timed-out wake that already had an irreversible/visible effect (no double-post on a re-run).
    pub fn acted_outward(&self) -> bool {
        !self.outward_seen.lock().unwrap().is_empty()
    }

    /// Write-gating: a file-write target must sit under a `writable_dirs` prefix and must
    /// never be `runlogs/`. The target is first **relativized** against the soul
    /// repo (file tools emit absolute paths, grounded vs OpenClaude 0.15.0) — anything that
    /// escapes the repo, or `..`-escapes one writable dir into another (e.g. `memory/../
    /// skills/`), is denied.
    fn write_target_allowed(&self, target: &str) -> bool {
        let Some(rel) = self.relativize(target) else {
            return false; // escaped the soul repo entirely
        };
        if rel.starts_with("runlogs/") {
            return false; // never — the record is harness-authored.
        }
        self.spec.writable_dirs.iter().any(|d| rel.starts_with(d))
    }

    /// Is this write target LONG-TERM memory (`memory/`)? Gated to org+ by `long_term_writable`.
    fn is_long_term_write(&self, target: &str) -> bool {
        self.relativize(target).is_some_and(|rel| rel.starts_with("memory/"))
    }

    /// Resolve a write `target` to a **repo-relative, `..`-collapsed** path, or `None` if it
    /// escapes the soul repo. With a `repo_root`, an absolute target is relativized against
    /// it (and a `..`-escape above the root fails `strip_prefix`); without one (unit tests),
    /// the target is treated as repo-relative and any `..` component is rejected outright.
    fn relativize(&self, target: &str) -> Option<String> {
        let t = Path::new(target);
        match &self.repo_root {
            Some(root) => {
                let root = normalize_lexical(root);
                let abs = if t.is_absolute() {
                    normalize_lexical(t)
                } else {
                    normalize_lexical(&root.join(t))
                };
                let rel = abs.strip_prefix(&root).ok()?;
                Some(rel.to_string_lossy().replace('\\', "/"))
            }
            None => {
                if t.components().any(|c| c == Component::ParentDir) {
                    return None;
                }
                Some(normalize_lexical(t).to_string_lossy().replace('\\', "/"))
            }
        }
    }
}

#[async_trait]
impl ActionResponder for StatePolicyResponder {
    async fn decide(&self, req: &ActionRequest) -> ActionDecision {
        // (0) Derive the class + target path from the raw (tool, input) event. Unknown
        //     tools classify as `Other` → default-denied by the scope check.
        let (class, target_path) = classify_tool(
            &req.tool,
            &req.input,
            &self.settle_tools,
            &self.post_tools,
            &self.read_tools,
        );

        // (1) Per-state class scoping — the first and primary gate.
        if !self.spec.tool_scope.allows(class) {
            // The duck has a strong coding-agent prior: it tries to DELEGATE by calling an
            // `Agent`/`Task` tool. For the duck no such tool exists (only a worker, whose `Other`
            // scope admits `Task`, ever reaches this allowed). Catch that exact confusion and teach
            // the real mechanism — the `spawn` OUTPUT FIELD — instead of a bare "not in scope".
            if is_delegation_tool(&req.tool) {
                return ActionDecision::Deny(format!(
                    "there is no `{}` tool for you — you do NOT spawn a worker by calling a tool. \
                     Delegation is a structured OUTPUT: in your Express step, return a `spawn` field \
                     in your result JSON — `spawn: {{ \"agent\": \"coder\", \"brief\": \"<the job>\" }}` \
                     — and stop. The harness then launches the keyless sandboxed worker for you, and \
                     its summary returns later as a new wake you judge. Do NOT try to build it \
                     yourself and do NOT keep searching for a tool; just return `spawn`.",
                    req.tool
                ));
            }
            return ActionDecision::Deny(format!(
                "tool `{}` (class {:?}) not in {:?} scope",
                req.tool, class, self.spec.state
            ));
        }

        // (1.5) DRY-RUN (testing): the tool is in scope, but a dry-run block prefix says don't
        // EXECUTE it. The agent already composed the action (it's recorded in the runlog) — the wall
        // just stops it landing. Tool-level, so a no-real-trade run still lets `simulate_swap`/`get_*`
        // through while holding `buy_token`. One mechanism for every MCP (no per-server env).
        if self.dry_run_block.iter().any(|prefix| req.tool.starts_with(prefix)) {
            // It's a Deny (the SDK's canUseTool can't return a synthetic success — only allow/deny),
            // so the message must do the work: tell the model this is INTENTIONAL test mode, the
            // action was recorded, and NOT to retry — so it moves on instead of flailing.
            return ActionDecision::Deny(format!(
                "dry-run test mode: `{}` was recorded but intentionally NOT executed. This is \
                 expected — it is NOT a failure and NOT a permissions problem. Do not retry it; \
                 treat the action as already done and continue (or stop).",
                req.tool
            ));
        }

        // (2) Write-gating: a file write must sit under the state's writable_dirs.
        if class == ToolClass::FileWrite {
            match &target_path {
                Some(path) if self.write_target_allowed(path) => {
                    // (2a) Long-term memory gate: `memory/` is the duck's LONG-TERM memory, writable only
                    // by an org+ cycle (a digest job / reflect). A lower-trust in-run cycle leaves tags on
                    // its baton; the digest consolidates — so no laundered note lands in self-trust memory.
                    if !self.long_term_writable && self.is_long_term_write(path) {
                        return ActionDecision::Deny(
                            "`memory/` is your LONG-TERM memory — only a trusted (org+) cycle, a digest \
                             job, or reflect may write it. Don't write it here; leave a tag on your baton \
                             (and a tag-note) and the digest will consolidate it into memory."
                                .into(),
                        );
                    }
                }
                Some(path) => {
                    return ActionDecision::Deny(format!(
                        "{:?} may not write `{path}`",
                        self.spec.state
                    ));
                }
                None => return ActionDecision::Deny("write with no target path".into()),
            }
        }

        // (2.5) Record OUTWARD actions (Post / SettleTx) so `acted_outward()` can gate the
        // timeout-retry path (never re-run a wake that already posted/traded → no double-post on a
        // re-run). We DO NOT block a verbatim-duplicate outward call. A repeat is usually a legitimate
        // RETRY after a send glitch — the wall ALLOWING a call is NOT the send SUCCEEDING (the MCP
        // result returns later, unseen here), so blocking a retry drops the real message AND misleads
        // the model into thinking it delivered (observed live: a reply to avanturismo silently lost).
        // The model is reliable (measured duplicate rate ~0.4%, single retries, no loops). Where an
        // irreversible/costly repeat genuinely must be prevented, that bound is EXTERNAL — the chain
        // rejects a replayed tx (nonce), the provider caps spend (cove hourly/daily limits) — not the
        // wall's job to guess intent. If you hold the key (the right state-prompt + access), you may
        // act, even identically; block it on-chain / at the provider, not here.
        if matches!(class, ToolClass::Post | ToolClass::SettleTx) {
            self.outward_seen
                .lock()
                .unwrap()
                .insert(format!("{}\u{1}{}", req.tool, req.input));
        }

        // (3) Irreversible authority needs NO extra runtime predicate (the `allow_settle`
        // whitelist+operator_signed reflex was removed — it was never wired). Being here means a
        // REGISTERED `tier: settle` capability is running IN Settle, and Settle is reachable only by
        // an UNCONTAMINATED cycle whose accumulated trust `reaches: settle` (the taint model). So
        // irreversibility is bounded UPSTREAM (taint-reachability + this state-scope gate + the
        // per-server tool allowlist) and EXTERNALLY (the custodial/wallet limits — cove daily caps /
        // no-withdraw, on-chain the wallet's native allowances). A settle-tier tool in Settle is
        // therefore allowed — it falls through to the final decision below.

        ActionDecision::Allow
    }
}

/// Is this an agent/subagent-spawning tool (the SDK `Task`, or the harness `Agent`)? Used only to
/// give a teaching deny when the DUCK reaches for one — a worker's `Other` scope admits these, so it
/// never hits the deny. Matches the bare tool name (any `mcp__…__` prefix stripped), case-insensitive.
fn is_delegation_tool(tool: &str) -> bool {
    let bare = tool.rsplit("__").next().unwrap_or(tool);
    bare.eq_ignore_ascii_case("Agent") || bare.eq_ignore_ascii_case("Task")
}

/// Lexically resolve `.`/`..` segments **without touching the filesystem** (a write target
/// may not exist yet, and we must not follow symlinks during a security check). `..` pops the
/// last kept component; `.` is dropped; roots/prefixes are kept.
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{default_spec, ConsciousnessState};
    use serde_json::json;

    fn caps() -> (Vec<CapabilityPrefix>, Vec<CapabilityPrefix>) {
        (
            vec![CapabilityPrefix::open("mcp__twitter__")],  // post tools
            ["mcp__bankr__", "mcp__dac__", "mcp__cove-trading__"]
                .iter()
                .copied()
                .map(CapabilityPrefix::open)
                .collect(), // settle tools
        )
    }

    /// Build a grounded permission event: raw tool name + JSON input.
    fn req(tool: &str, input: serde_json::Value) -> ActionRequest {
        ActionRequest {
            tool: tool.into(),
            tool_use_id: "tu-1".into(),
            input,
        }
    }

    fn responder(state: ConsciousnessState) -> StatePolicyResponder {
        let (post, settle) = caps();
        StatePolicyResponder::with_capabilities(default_spec(state), post, settle)
    }

    #[tokio::test]
    async fn long_term_memory_write_is_gated_by_trust() {
        // Express may write memory/ — but only when the cycle clears the long-term floor (org+).
        // A below-floor cycle (`long_term_writable = false`) is DENIED `memory/`; an org+ cycle is allowed.
        let (post, settle) = caps();
        let mut low = StatePolicyResponder::with_capabilities(default_spec(ConsciousnessState::Express), post.clone(), settle.clone());
        low.long_term_writable = false;
        assert!(matches!(
            low.decide(&req("Write", json!({"file_path": "memory/social.md", "contents": "x"}))).await,
            ActionDecision::Deny(d) if d.contains("LONG-TERM")
        ), "a below-floor cycle cannot write long-term memory");

        let mut ok = StatePolicyResponder::with_capabilities(default_spec(ConsciousnessState::Express), post, settle);
        ok.long_term_writable = true;
        assert!(matches!(
            ok.decide(&req("Write", json!({"file_path": "memory/social.md", "contents": "x"}))).await,
            ActionDecision::Allow
        ), "an org+ cycle (digest/reflect) writes long-term memory");
    }

    #[tokio::test]
    async fn perceive_cannot_post_or_write() {
        let r = responder(ConsciousnessState::Perceive);
        assert!(matches!(
            r.decide(&req("mcp__twitter__post", json!({"text": "hi"}))).await,
            ActionDecision::Deny(_)
        ));
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "memory/log.md", "contents": "x"})))
                .await,
            ActionDecision::Deny(_)
        ));
        // And raw Bash is denied (it would bypass path-gating).
        assert!(matches!(
            r.decide(&req("Bash", json!({"command": "echo hi > skills/x"}))).await,
            ActionDecision::Deny(_)
        ));
    }

    #[tokio::test]
    async fn express_can_post_and_write_memory_only() {
        let r = responder(ConsciousnessState::Express);
        assert!(matches!(
            r.decide(&req("mcp__twitter__post", json!({"text": "gm"}))).await,
            ActionDecision::Allow
        ));
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "memory/log.md", "contents": "x"})))
                .await,
            ActionDecision::Allow
        ));
        // but cannot write soul dirs (FileWrite is in scope, the PATH is not).
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "skills/x/SKILL.md", "contents": "x"})))
                .await,
            ActionDecision::Deny(_)
        ));
        // and cannot run raw Bash (Shell never in scope).
        assert!(matches!(
            r.decide(&req("Bash", json!({"command": "curl evil"}))).await,
            ActionDecision::Deny(_)
        ));
    }

    /// Duplicate outward actions are ALLOWED: a verbatim repeat is usually a legit retry after a send
    /// glitch (blocking it drops the real message + misleads the model), and irreversibility is bounded
    /// externally (chain nonce + provider limits), not by the wall. The fingerprint is still recorded so
    /// `acted_outward()` keeps guarding the timeout-retry path.
    #[tokio::test]
    async fn duplicate_outward_action_is_allowed_but_records_acted_outward() {
        let r = responder(ConsciousnessState::Express);
        let post = req("mcp__twitter__post", json!({"text": "the harness blinked"}));
        // First time: allowed.
        assert!(matches!(r.decide(&post).await, ActionDecision::Allow));
        // Verbatim repeat: NOW allowed too (a retry, not a loop-block).
        assert!(matches!(r.decide(&post).await, ActionDecision::Allow));
        // A distinct post: allowed.
        assert!(matches!(
            r.decide(&req("mcp__twitter__post", json!({"text": "different thought"}))).await,
            ActionDecision::Allow
        ));
        // …but the outward action WAS recorded → the timeout-retry guard still refuses to re-run it.
        assert!(r.acted_outward(), "an allowed post flips acted_outward (no double-post on timeout re-run)");
    }

    /// `acted_outward` is the timeout-retry safety signal: it flips true ONLY once an outward action
    /// (Post / SettleTx) is actually allowed — a read or a memory write must NOT set it (those are safe
    /// to re-run after a hang).
    #[tokio::test]
    async fn acted_outward_flips_only_after_an_allowed_outward_action() {
        let r = responder(ConsciousnessState::Express);
        assert!(!r.acted_outward(), "clean before any action");
        // A memory write is not outward → still clean.
        let w = req("Write", json!({"file_path": "memory/log.md", "contents": "x"}));
        assert!(matches!(r.decide(&w).await, ActionDecision::Allow));
        assert!(!r.acted_outward(), "a memory write is not an outward action");
        // An allowed Post is outward → now flipped (so a later timeout won't be retried).
        let post = req("mcp__twitter__post", json!({"text": "hi"}));
        assert!(matches!(r.decide(&post).await, ActionDecision::Allow));
        assert!(r.acted_outward(), "an allowed post is an outward action");
    }

    /// The WORKER sandbox: a worker may Bash + spawn sub-helpers (`Agent`/`Task`) + write
    /// its `/workspace`, but has NO outward authority (Post/Settle) and CANNOT write the soul repo.
    #[tokio::test]
    async fn worker_spec_sandbox_allows_workspace_denies_soul_and_outward() {
        let ws = "/tmp/dack-ws-test";
        // Configure the post/settle prefixes so they CLASSIFY — then the worker scope (which omits
        // Post/SettleTx) is what denies them (a real worker also has mcp_servers={}, a second guard).
        let (post, settle) = caps();
        let r = StatePolicyResponder::with_capabilities(crate::state::worker_spec(), post, settle)
            .with_repo_root(ws);
        // Bash + sub-helper spawn (Agent/Task = Other) are IN a worker's scope.
        assert!(matches!(r.decide(&req("Bash", json!({"command": "cargo build"}))).await, ActionDecision::Allow));
        assert!(matches!(r.decide(&req("Agent", json!({"description": "research X"}))).await, ActionDecision::Allow));
        // It writes its workspace...
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "/tmp/dack-ws-test/src/main.rs", "contents": "fn main(){}"}))).await,
            ActionDecision::Allow
        ));
        // but NOT the soul repo (outside the workspace → denied)...
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "/tmp/dack-soul/SOUL.md", "contents": "x"}))).await,
            ActionDecision::Deny(_)
        ));
        // and has NO outward authority (no Post/Settle in scope).
        assert!(matches!(r.decide(&req("mcp__twitter__post", json!({"text": "as the duck"}))).await, ActionDecision::Deny(_)));
        assert!(matches!(r.decide(&req("mcp__cove-trading__buy_token", json!({}))).await, ActionDecision::Deny(_)));
    }

    /// When the DUCK (here Express) reaches for an `Agent`/`Task` tool to delegate — the strong
    /// coding-agent prior — the deny doesn't just say "not in scope"; it teaches the real mechanism:
    /// return the `spawn` output field. (A worker, by contrast, is ALLOWED these — tested above.)
    #[tokio::test]
    async fn duck_agent_call_is_denied_with_spawn_field_guidance() {
        let r = responder(ConsciousnessState::Express);
        for tool in ["Agent", "Task", "mcp__whatever__Task"] {
            match r.decide(&req(tool, json!({"description": "build solution.py"}))).await {
                ActionDecision::Deny(m) => {
                    assert!(m.contains("spawn"), "{tool}: deny should teach the spawn field: {m}");
                    assert!(!m.contains("class Other"), "{tool}: should be the teaching deny, not the bare one: {m}");
                }
                other => panic!("{tool}: expected a teaching Deny, got {other:?}"),
            }
        }
        // A genuinely-unrelated out-of-scope tool still gets the plain scope deny (no spawn noise).
        match r.decide(&req("SomeRandomTool", json!({}))).await {
            ActionDecision::Deny(m) => assert!(!m.contains("spawn"), "unrelated tool: {m}"),
            other => panic!("expected a Deny, got {other:?}"),
        }
    }

    /// Dry-run (testing): the wall denies the configured tool PREFIXES (composed, not executed) but
    /// leaves everything else — incl. reads and `simulate_swap` — alone. Tool-level, not class-level.
    #[tokio::test]
    async fn dry_run_block_denies_listed_prefixes_only() {
        let mut r = responder(ConsciousnessState::Express);
        r.dry_run_block = vec!["mcp__twitter__post".into(), "mcp__cove-trading__buy_token".into()];
        // A blocked outward tool → denied with a clear dry-run message (before the dedup guard).
        match r.decide(&req("mcp__twitter__post", json!({"text": "hi"}))).await {
            ActionDecision::Deny(m) => assert!(m.contains("dry-run"), "dry-run message: {m}"),
            other => panic!("expected dry-run Deny, got {other:?}"),
        }
        // A non-blocked tool (memory write) still passes — dry-run is tool-level, not a blanket halt.
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "memory/log.md", "contents": "x"})))
                .await,
            ActionDecision::Allow
        ));
        // A reply (not in the block list) still passes — only the listed prefixes are held.
        assert!(matches!(
            r.decide(&req("mcp__twitter__reply", json!({"text": "yo", "in_reply_to_tweet_id": "1"})))
                .await,
            ActionDecision::Allow
        ));
    }

    #[tokio::test]
    async fn reflect_writes_soul_but_not_runlogs() {
        let r = responder(ConsciousnessState::Reflect);
        // Reflect may write soul dirs...
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "skills/new/SKILL.md", "contents": "x"})))
                .await,
            ActionDecision::Allow
        ));
        // but NEVER runlogs/ (harness authors the record).
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "runlogs/2026-05-30.md", "contents": "x"})))
                .await,
            ActionDecision::Deny(_)
        ));
    }

    #[tokio::test]
    async fn settle_tier_tool_allowed_in_settle_by_tool_tier() {
        // A registered settle-tier tool, reached in Settle, is allowed — no extra predicate. The
        // bound is external (the funded cove balance + cove's own limits); reaching Settle required
        // an uncontaminated cycle (the taint model).
        let r = responder(ConsciousnessState::Settle);
        assert!(matches!(
            r.decide(&req("mcp__cove-trading__buy", json!({"token": "DOGE", "usd": "1"})))
                .await,
            ActionDecision::Allow
        ));
    }

    #[tokio::test]
    async fn read_server_allowlist_denies_offlist_tool() {
        // cove-read (read tier) serves a full surface incl. buy_token on the read-only token; its
        // `tools` allowlist holds it to read tools. In Perceive, a whitelisted read tool is allowed
        // (Read is in scope everywhere); a non-listed trade tool under the SAME prefix is denied
        // (fail-closed to Other) — it can NEVER ride the read tier into a reversible state.
        let mut r = responder(ConsciousnessState::Perceive);
        r.read_tools = vec![CapabilityPrefix {
            prefix: "mcp__cove-read__".into(),
            tools: vec!["get_balance".into(), "scan_trending_tokens".into()],
        }];
        assert!(matches!(
            r.decide(&req("mcp__cove-read__get_balance", json!({}))).await,
            ActionDecision::Allow
        ));
        assert!(matches!(
            r.decide(&req("mcp__cove-read__buy_token", json!({"token": "DOGE", "usd": "1"}))).await,
            ActionDecision::Deny(_)
        ));
    }

    #[tokio::test]
    async fn settle_tier_tool_denied_outside_settle() {
        // The state gate: a settle-class tool in Express is denied by SCOPE (Express does not scope
        // SettleTx). (And the harness never even exposes a settle-tier MCP outside Settle.)
        let r = responder(ConsciousnessState::Express);
        assert!(matches!(
            r.decide(&req("mcp__cove-trading__buy", json!({"token": "DOGE", "usd": "1"})))
                .await,
            ActionDecision::Deny(_)
        ));
    }

    // ── live absolute-path relativization (the SDK emits absolute file_paths) ──

    #[tokio::test]
    async fn relativizes_absolute_paths_and_blocks_escapes() {
        let r = responder(ConsciousnessState::Express).with_repo_root("/repo");
        // The live case: an absolute memory path under the repo → allowed.
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "/repo/memory/log.md", "content": "x"})))
                .await,
            ActionDecision::Allow
        ));
        // Absolute soul-dir path → denied (Express may write only memory/).
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "/repo/skills/x/SKILL.md", "content": "x"})))
                .await,
            ActionDecision::Deny(_)
        ));
        // `..`-escape out of memory/ into skills/ → denied (the path the prefix check missed).
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "/repo/memory/../skills/x", "content": "x"})))
                .await,
            ActionDecision::Deny(_)
        ));
        // Outside the soul repo entirely → denied.
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "/etc/passwd", "content": "x"})))
                .await,
            ActionDecision::Deny(_)
        ));
        // runlogs/ never — even via an absolute path.
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "/repo/runlogs/2026-06.md", "content": "x"})))
                .await,
            ActionDecision::Deny(_)
        ));
    }

    #[tokio::test]
    async fn reflect_writes_soul_dirs_via_absolute_paths() {
        let r = responder(ConsciousnessState::Reflect).with_repo_root("/repo");
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "/repo/skills/new/SKILL.md", "content": "x"})))
                .await,
            ActionDecision::Allow
        ));
        // but a double `..` above the repo root is still denied.
        assert!(matches!(
            r.decide(&req("Write", json!({"file_path": "/repo/../../etc/x", "content": "x"})))
                .await,
            ActionDecision::Deny(_)
        ));
    }
}
