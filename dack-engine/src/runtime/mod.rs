//! Runtime seam — the OpenClaude substrate, rented not rebuilt (architecture
//! **Engine = consciousness substrate; this harness = the client/approver = the
//! actor-scheduler.** Transport is **NDJSON over stdio** to a child Node bridge
//! ([`openclaude::OpenClaudeClient`] drives `openclaude-bridge/bridge.ts`); the operator/actor
//! split becomes a *process boundary*, not a convention. (OpenClaude also ships a gRPC server —
//! the original topology — but it under-exposes the engine; we chose its richer SDK + stdio.)
//!
//! The two load-bearing pieces:
//!   - [`RuntimeClient`] — invoke a consciousness state with an assembled context and
//!     a per-state allowed-tool set; returns the agent's structured [`AgentOutput`].
//!   - [`ActionResponder`] — **the wall**. Every tool call routes through the SDK's
//!     `canUseTool(name, input, {toolUseID})` callback, which the bridge relays to this
//!     out-of-process responder for a y/n decision before the tool runs. The agent can't touch it.
//!
//! verified: `canUseTool` fires for *all* tool classes (bash, file-write, network, MCP, subagent)
//! with no bypass. The approval channel is the child's stdin/stdout — a pipe binds nothing,
//! so there is no socket to impersonate.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::Result;
use crate::model::proposal::AgentOutput;
use crate::state::StateSpec;

pub mod action_required;
pub mod classify;
pub mod claude_cli;
pub mod openclaude;

/// An engine session handle (OpenClaude `Query.sessionId` / vision). Reusing a
/// session preserves context across wakes — cheaper, more coherent ("reads the room"),
/// and the substrate for coalescing tweets one-by-one within one context and periodic
/// compaction.
///
/// **Firebreak guardrail (non-negotiable):** a session may be reused only *within a
/// trust-homogeneous lane*. The Perceive→Express/Settle boundary MUST be a fresh session
/// — never hand Express the session that ingested untrusted payload, or the raw bytes
/// leak across the firebreak. Same-state self-authored wakes (e.g. a Perceive
/// heartbeat lane, or an Express posting lane) may share a session; a public-payload
/// Perceive run feeding Express may not.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

/// Token accounting for one invocation (from the engine's usage report). `input_tokens` +
/// `cache_read_input_tokens` ≈ the CONTEXT size the model just processed — which, on a sticky resume,
/// is the whole accumulated session. The harness thresholds on `context_tokens()` to size-evict a
/// session before it bloats (cost + degradation + confabulation). Extra engine fields are ignored.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct InvokeUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl InvokeUsage {
    /// The resumed context size (fresh input + cache-read input). Cache-creation/output are excluded —
    /// they're not "context already in the window".
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.cache_read_input_tokens)
    }
}

/// A delimited block of context assembled for an invocation. The `trusted`
/// flag drives the *visible* framing: trusted briefing vs untrusted world. Keeping
/// directive (trusted) and payload (untrusted) as distinct blocks is the rule
/// carried into context assembly.
#[derive(Debug, Clone)]
pub struct ContextBlock {
    pub label: String,
    pub body: String,
    pub trusted: bool,
}

/// Everything needed to run one consciousness state.
#[derive(Debug, Clone)]
pub struct InvocationRequest {
    pub spec: StateSpec,
    /// `SOUL.md` + state prompt. Delivered as the SDK's `systemPrompt: {type:'custom'}`
    /// (verified public SDK option) so the soul IS the system prompt — not OpenClaude's
    /// default coding-assistant prompt, and not smuggled into the user turn.
    pub system_prompt: String,
    /// Ordered context blocks (directive trusted; coalesced payload untrusted; memory
    /// retrieval summary; recent runlog tail).
    pub blocks: Vec<ContextBlock>,
    /// Reuse an existing engine session for context continuity, or `None` for a fresh
    /// context. **MUST be `None` across the firebreak** (see [`SessionId`]). The degraded
    /// `claude -p` adapter ignores this (always fresh).
    pub session: Option<SessionId>,
    /// The agent's working directory — the **soul repo** — so its `Read`/`Write`/`Glob` tools
    /// operate on `memory/`, `skills/`, … and emit absolute paths under it that the wall
    /// relativizes. `None` = the bridge's own cwd (tests / pure-text runs).
    pub workdir: Option<PathBuf>,
    /// Per-invocation secret env, **materialized by the harness** for the act phase (the
    /// skills the agent calls read it, e.g. `X_BEARER_TOKEN` to post). Operator-gated via the
    /// route's `secrets:`; **empty for Perceive** (the read-only state holds no act creds).
    /// Overlaid on the bridge's static env at spawn.
    pub secret_env: BTreeMap<String, String>,
    /// Resolved MCP **capability** servers for this invocation, keyed by server name,
    /// each an SDK-shaped config (`{type:"http",url,headers}` or `{type:"stdio",command,args,env}`)
    /// with the auth token **already injected** by the harness into headers/env — so the token
    /// reaches the server but NEVER the agent's context. The harness picks these per state (the
    /// route's `capabilities:` ∩ the state's tier); the bridge sets `options.mcpServers` verbatim.
    pub mcp_servers: BTreeMap<String, serde_json::Value>,
    /// Per-invocation model override (8.7) — the harness-resolved effective model for THIS state-
    /// prompt (operator `tier_policy` default, or a soul `model:` where override is allowed). `None`
    /// ⇒ the runtime client's configured `config.model`. The bridge maps it to `options.model`.
    pub model: Option<String>,
    /// **Sub-agent definitions** the engine may spawn via the `Task` tool, keyed by name
    /// → an SDK `options.agents` value ([`crate::agent_def::AgentDef::to_options_value`]). EMPTY for
    /// the duck's consciousness states (no `Task` target); a **worker** invocation registers its
    /// sub-helpers here so it can delegate plan/research/QA in-session. The bridge sets `options.agents`.
    pub agents: std::collections::BTreeMap<String, serde_json::Value>,
    /// **OS-isolate this invocation**: run the bridge in the runtime's worker sandbox
    /// (Docker) instead of on the host. Set ONLY for delegated workers; the duck's states leave it
    /// `false` (the duck is never containerized). No-op if the runtime has no worker backend
    /// configured (falls back to host) — see `runtime.worker_sandbox`.
    pub isolate: bool,
    /// **Extra read-only mounts** for an isolated worker: the agent def's resolved
    /// `volumes:` (host soul-subdir → guest path), all read-only. The writable `/workspace` is NOT
    /// here — it's derived from `workdir`. Empty for the duck.
    pub mounts: Vec<crate::sandbox::Mount>,
    /// **Explicit tool allow-list** for the engine (`options.allowedTools`). `Some` pins the engine to
    /// exactly these tools — a WORKER sets it to its agent def's `tools:` so the SDK does NOT offer its
    /// full default toolset (some of which Docker-sandbox sub-work and die inside a container with no
    /// docker daemon). `None` (the duck) ⇒ the engine default; the wall gates every call regardless.
    pub allowed_tools: Option<Vec<String>>,
    /// **Per-invocation wall-clock budget OVERRIDE.** `None` ⇒ the runtime client's configured default
    /// (`invoke_timeout_secs`). The harness sets `Some` for a state that legitimately needs longer —
    /// e.g. **Reflect** (`reflect_invoke_timeout_secs`), the daily self-modification cycle, which may
    /// chew through prompts/skills/memory and deserves more than a chat reply's budget.
    pub timeout: Option<std::time::Duration>,
}

/// The permission event surfaced by OpenClaude, as it *actually* arrives (grounded
/// against 0.15.0): the SDK `canUseTool(name, input, {toolUseID})` callback — a raw tool
/// name + JSON input, NOT a pre-classified event. The responder derives the class via
/// [`classify`]. (The stock gRPC proto's `ActionRequired` is even leaner — only a
/// `prompt_id` + "Approve <tool>?" — so against it we correlate with the preceding
/// `tool_start` to recover `(name, input, tool_use_id)`; the SDK gives them directly.)
#[derive(Debug, Clone)]
pub struct ActionRequest {
    /// The concrete tool the engine wants to run (e.g. "Write", "mcp__twitter__post").
    pub tool: String,
    /// Correlation id — the SDK's `toolUseID`; the decision is returned via
    /// `query.respondToPermission(tool_use_id, …)`.
    pub tool_use_id: String,
    /// Raw tool arguments. The responder reads this to derive class + target path, and the
    /// runlog records a truncated rendering so the audit shows WHAT the duck did.
    pub input: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum ActionDecision {
    Allow,
    Deny(String),
}

/// The wall seam. Implemented by [`action_required::StatePolicyResponder`].
#[async_trait]
pub trait ActionResponder: Send + Sync {
    async fn decide(&self, req: &ActionRequest) -> ActionDecision;
}

/// Invoke a consciousness state. The responder is consulted on every sensitive tool
/// call mid-run (the `action_required` round-trip). Returns the agent's structured output AND the
/// engine `session_id` (when the engine reports one) — the harness persists it for sticky-session
/// resume (`InvocationRequest.session`). `None` when the engine/adapter has no session concept.
#[async_trait]
pub trait RuntimeClient: Send + Sync {
    async fn invoke(
        &self,
        req: InvocationRequest,
        responder: std::sync::Arc<dyn ActionResponder>,
    ) -> Result<(AgentOutput, Option<SessionId>, Option<InvokeUsage>)>;

    /// The guest working dir a containerized worker runs at (e.g. `/workspace`), or `None` if this
    /// runtime has no worker isolation backend. The harness calls this to decide whether a
    /// `docker`-isolation worker will ACTUALLY be containerized — so it can root the worker's wall at
    /// the matching path (the guest `/workspace` when isolated, the host workspace when falling back).
    /// Default `None` (mocks / host-only runtimes).
    fn worker_guest_cwd(&self) -> Option<PathBuf> {
        None
    }
}
