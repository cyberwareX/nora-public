//! Claude Code one-shot adapter — the **swappable degraded runtime** (`claude -p
//! "<prompt>"`), the Hermes-style stateless pattern. Live-verified during grounding:
//! `claude -p "…"` returns a completion end-to-end.
//!
//! This is the proof that the [`RuntimeClient`](super::RuntimeClient) seam is real: the
//! same harness drives either OpenClaude (session-ful, live `canUseTool` approval) or a
//! plain `claude -p` one-shot. The corp variant lands here too.
//!
//! **Two honest limitations of one-shot mode**, both consistent with the architecture:
//! 1. **No live approval round-trip.** `claude -p` decides permissions from flags
//!    (`--allowedTools` / `--disallowedTools` / `--permission-mode`), not a callback. So
//!    per-state scoping is delivered as a *pre-declared* tool allow/deny list, and there
//!    is no dynamic permission callback mid-run.
//! 2. **⇒ Settle is unsupported in one-shot mode** — the irreversibility wall needs the
//!    live callback runtime. This is fine: degraded mode is for reversible shitposting;
//!    anything irreversible requires the OpenClaude (or equivalent callback) runtime.
//!
//! It is also strictly firebreak-*safe*: every call is a fresh context (no session), so
//! no untrusted payload can persist across the Perceive→Express boundary.
//!
//! SCAFFOLD: spawns `claude -p` with the per-state allow/deny list in.

use async_trait::async_trait;
use std::sync::Arc;

use super::{ActionResponder, InvocationRequest, RuntimeClient};
use crate::error::Result;
use crate::model::proposal::AgentOutput;

pub struct ClaudeCliClient {
    /// Path to the `claude` (or `openclaude`) binary.
    pub binary: String,
}

impl Default for ClaudeCliClient {
    fn default() -> Self {
        Self {
            binary: "claude".to_string(),
        }
    }
}

#[async_trait]
impl RuntimeClient for ClaudeCliClient {
    async fn invoke(
        &self,
        _req: InvocationRequest,
        _responder: Arc<dyn ActionResponder>,
    ) -> Result<(AgentOutput, Option<super::SessionId>, Option<super::InvokeUsage>)> {
        todo!(
            "spawn `claude -p <assembled-prompt>` with --append-system-prompt for \
             the SOUL/state prompt and --disallowedTools derived from the state scope; parse \
             the one-shot result into AgentOutput. The `responder` is consulted to BUILD the \
             static allow/deny list (no live round-trip); SettleTx is never permitted here."
        )
    }
}
