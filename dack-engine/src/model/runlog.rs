//! The RunLog entry. **Harness-authored — the agent can never write it.**
//! Rationale (keep this; an implementer will be tempted to let the agent log itself):
//! if the agent could write its own runlog, a compromised agent could erase the
//! evidence of its own compromise. The record must be authored by the layer that is
//! not under attack.
//!
//! Append-only, one file per day (`runlogs/YYYY-MM-DD.md`). Errors are entries tagged
//! `error` — there is no separate ErrorLog store; Reflect reviews the day's runlogs
//! (including errors) and decides what to learn.

use serde::{Deserialize, Serialize};

use super::baton::Baton;
use super::proposal::AgentOutput;
use super::stimulus::StimulusId;
use crate::state::ConsciousnessState;

/// One `action_required` decision, recorded so an injection path is visible post-hoc
/// and becomes a lesson.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub tool: String,
    /// "allow" | "deny: <reason>".
    pub decision: String,
    /// A compact, truncated rendering of the tool input — so the runlog audit shows **what** the
    /// duck did (the reply text it posted, the path it wrote), not merely which tool it reached.
    #[serde(default)]
    pub input: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome", content = "detail")]
pub enum Outcome {
    Ok,
    /// Errors are RunLog entries tagged `error` — not a separate store.
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLogEntry {
    /// Anchor for `runlog_ref`, e.g. "run-0412".
    pub run_id: String,
    pub stimulus_id: StimulusId,
    /// The duty/source id that emitted this cycle's stimulus (e.g. `telegram-op`, `twitter-mentions`);
    /// `harness*` for synthesized wakes (reflect, back-online, workers). Feeds the per-source runlog stats.
    #[serde(default)]
    pub source: String,
    pub state: ConsciousnessState,
    /// A summary of the assembled invocation context.
    pub context_summary: String,
    /// The input→proposal (Baton) mapping, so an injection path is visible post-hoc.
    #[serde(default)]
    pub baton: Option<Baton>,
    /// Raw stimulus, stored in a **clearly-delimited-untrusted** block — this is what
    /// `runlog_ref` points at.
    pub raw_stimulus: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(default)]
    pub output: Option<AgentOutput>,
    pub outcome: Outcome,
    pub timestamp: i64,
    /// Conversation/topic tags for context-scoped recall: the conversation key (when the prompt opts in
    /// via `tag_key`) plus any baton-carried tags. A resumed sticky session filters the runlog diff to
    /// its own tag so parallel chats don't bloat its context. Empty = untagged (only in the global view).
    #[serde(default)]
    pub tags: Vec<String>,
}
