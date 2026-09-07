//! **State-prompts** — the soul-owned chain-of-thought. A state-prompt is a
//! named variant of a consciousness state: `(state-tier, prompt body, MCP plan, allowed
//! transitions)`. They live in a **nested filesystem tree mirroring `stimuli/`** — e.g.
//! `prompts/settle.md`, `prompts/twitter/perceive_mention.md`, `prompts/twitter/feed_reply.md` —
//! and a state-prompt's **id is its path relative to `prompts/`, without the extension**
//! (`twitter/perceive_mention`).
//!
//! The split: the **operator owns the WALL** (which consciousness
//! tiers exist, their reversibility, and — via `tier_policy` — which MCP classes a tier admits); the
//! **soul owns the PATH** (which state-prompts run, in what order, and which *permitted* MCPs each
//! plugs). The frontmatter here is the soul's half; it can only ever REQUEST a capability — the
//! operator's `tier_policy` and the wall decide whether it is granted.
//!
//! **frontmatter is config; the body is the directive text** (trusted), exactly as in
//! `stimuli/`. The harness reads a state-prompt **live** from the soul repo each time it is opened,
//! so a Reflect edit takes effect on the next wake.

use serde::{Deserialize, Serialize};

use crate::error::{DackError, Result};
use crate::state::ConsciousnessState;
use crate::stimuli::split_frontmatter;

/// One MCP the soul asks a state-prompt to plug. Either a **bare ref** (a string naming an
/// operator-registered `mcp_servers` entry to import — its token stays operator-side, never in the
/// agent context) or an **inline** public MCP `{name, url}` the soul adds itself (no secret →
/// **forced read-tier** by the harness; a soul can never inline a post/settle tool). The
/// `tier_policy` for the state decides whether either form is actually admitted (I16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpRef {
    /// Import an operator-registered server by `name` (token injected by the harness broker).
    Import(String),
    /// A public, secret-less MCP the soul declares inline — forced to read-tier on assembly.
    Inline { name: String, url: String },
}

impl McpRef {
    /// The server name this ref resolves to (the import name, or the inline `name`).
    pub fn name(&self) -> &str {
        match self {
            McpRef::Import(n) => n,
            McpRef::Inline { name, .. } => name,
        }
    }
}

/// Opt-in **sticky session** for a state-prompt (resume-by-id): the engine session is kept and
/// resumed across items that share the same key, so the prompt accumulates context (e.g. all replies
/// in one thread) instead of re-paying it per item. The session key is always `(prompt-id, cycle
/// taint, …key dims)` — `key` adds the extra dimensions resolved from the stimulus (`thread_id` →
/// the stimulus `dedup_key`/`conversation_id`). `sticky: false`/absent = a fresh session each run
/// (the firebreak default). The firebreak still holds: a *different* state-prompt is a different key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    #[serde(default)]
    pub sticky: bool,
    /// Extra key dimensions beyond `(prompt-id, taint)` — e.g. `[thread_id]`. Empty = sticky per
    /// (prompt, taint). Not capped: any number of dims, each resolved from the stimulus.
    #[serde(default)]
    pub key: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_environment() -> usize {
    10
}

fn default_thread_depth() -> usize {
    10
}

fn default_thread_recent_lookback() -> i64 {
    3600
}

fn default_recent_notes() -> usize {
    20
}

/// How much runlog to inject, as TWO independent views (the de-bloat for sticky sessions):
/// - `environment`: the GLOBAL recent tail (last N), FRESH wakes only — broad orientation across all
///   activity. `0` = off. Dropped on a resume (the session already carries it; it's broad noise there).
/// - `conversation`: the THIS-conversation view (entries tagged with the wake's `dedup_key`) — the recent
///   tail on a FRESH wake, the **diff since last wake** on a resume. `0` = off. Needs `tag_key: true` to
///   have anything tagged to filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunlogContext {
    /// Show the fresh-wake `environment` map (short-term-memory digest); days of history it
    /// summarizes (`0` = off). On a resume it's replaced by the lean `environment-recent` diff.
    #[serde(default = "default_environment")]
    pub environment: usize,
    /// Fresh-wake `thread` block: the last N **full safe entries** (thought + gist + batons + tool
    /// audit, raw payload excluded) of the current thread (`0` = off; needs a thread key `tag_key`).
    #[serde(default = "default_thread_depth")]
    pub thread: usize,
    /// Resume `thread-recent` block: include this thread's entries since `max(watermark, now - this)`
    /// seconds — the "since you last woke, but at most this far back" diff.
    #[serde(default = "default_thread_recent_lookback")]
    pub thread_recent_lookback_secs: i64,
    /// `environment` block: how many recent sticky-notes (newest-first, tag + trust + text) to show —
    /// the duck's cross-thread short-term memory. **`0` = unlimited** (inject every note; the catalogue
    /// is itself capped, so this is bounded — used by the digest so no fact is collapsed away).
    #[serde(default = "default_recent_notes")]
    pub recent_notes: usize,
    /// Collapse recent notes to the LATEST per tag (with a `[+N more]` counter) vs show EVERY note in
    /// recency order. Chats want `true` (broad — one note per thread, small cap covers many); the digest
    /// wants `false` (see all the facts, nothing collapsed) so it can consolidate them into `memory/`.
    #[serde(default = "default_true")]
    pub dedup_notes: bool,
}

impl Default for RunlogContext {
    fn default() -> Self {
        Self {
            environment: default_environment(),
            thread: default_thread_depth(),
            thread_recent_lookback_secs: default_thread_recent_lookback(),
            recent_notes: default_recent_notes(),
            dedup_notes: true,
        }
    }
}

/// What context blocks a state-prompt wants injected (de-bloat knobs; resume-awareness is automatic).
/// `memory` injects the memory tail on a FRESH wake only (never re-sent on a sticky resume — the session
/// already carries it; the duck `Read`s `memory/` on demand). `tag_key` auto-tags this prompt's runlog
/// entries with the conversation key (`dedup_key`) so the `conversation` view can filter to them. `runlog`
/// is the two-view config above.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_true")]
    pub memory: bool,
    #[serde(default)]
    pub tag_key: bool,
    #[serde(default)]
    pub runlog: RunlogContext,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self { memory: true, tag_key: false, runlog: RunlogContext::default() }
    }
}

/// The YAML frontmatter of a `prompts/**/*.md` state-prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatePromptFrontmatter {
    /// The consciousness tier this prompt runs in (operator-bounded — the wall + ceiling gate it).
    pub state: ConsciousnessState,
    /// The MCPs the soul requests for this prompt (subject to the two-sided handshake). Empty = none.
    #[serde(default)]
    pub mcp: Vec<McpRef>,
    /// The **allowed set** of next state-prompt ids. Each run picks **exactly one** (or terminates);
    /// the chosen target's `state` tier is ceiling-checked before it opens. Empty = terminal.
    #[serde(default)]
    pub transitions: Vec<String>,
    /// Optional per-prompt model override (the soul's half of the model handshake). Honored ONLY
    /// where the operator's `tier_policy[state].allow_model_override` is set; otherwise ignored in
    /// favour of the operator's per-state default, then the global `config.model` (I16 shape).
    #[serde(default)]
    pub model: Option<String>,
    /// Optional sticky-session config (resume-by-id). `None`/`sticky:false` = a fresh session per run.
    #[serde(default)]
    pub session: Option<SessionConfig>,
    /// The payload-item FIELD a baton's `reply_to` is matched against — the reply identifier the model
    /// copies from `payload.items` (e.g. `message_id`). The harness validates `reply_to` against this
    /// field of the batch it holds (the firebreak), then resolves the reply destination from the
    /// matched item. `None` = the platform-agnostic default (`message_id`, then `id`). Generic: a
    /// twitter prompt would set `reply_key: id`.
    #[serde(default)]
    pub reply_key: Option<String>,
    /// De-bloat knobs for the injected context blocks (memory/runlog). `None` ⇒ both on.
    #[serde(default)]
    pub context: Option<ContextConfig>,
}

/// A parsed state-prompt: its id (path), frontmatter, and the trusted directive body.
#[derive(Debug, Clone)]
pub struct StatePrompt {
    /// Path relative to `prompts/`, without extension — e.g. `twitter/perceive_mention`.
    pub id: String,
    pub state: ConsciousnessState,
    pub mcp: Vec<McpRef>,
    pub transitions: Vec<String>,
    /// Soul-requested model override (operator-gated at assembly). `None` = the configured default.
    pub model: Option<String>,
    /// Sticky-session opt-in (resume-by-id). `None` = a fresh session each run (firebreak default).
    pub session: Option<SessionConfig>,
    /// The reply-identifier field a baton's `reply_to` is matched against (default `message_id`→`id`).
    pub reply_key: Option<String>,
    /// De-bloat knobs for the injected context blocks (memory/runlog). `None` ⇒ both on (`ContextConfig::default`).
    pub context: Option<ContextConfig>,
    /// **Frame 1 — the SYSTEM frame.** The state's STABLE invariant rules (e.g. "you are in Express;
    /// reply-only; one outward action"), appended to SOUL.md in the system message. Identical every
    /// turn — fresh AND resume — so it stays in the cached prefix. The content of the prompt file ABOVE
    /// the `---task---` marker (empty if there is no marker). Trusted.
    pub system_frame: String,
    /// **Frame 2 — the FIRST-USER frame.** The verbose state teaching (how-to, voice, examples), sent
    /// once as part of the user message on a FRESH wake. It then lives in the replayed conversation
    /// history, so a resume need not re-teach it. The content BETWEEN `---task---` and `---resume---`.
    /// Trusted.
    pub body: String,
    /// **Frame 3 — the RESUME frame.** The LEAN continuation cue used in the user message on a
    /// sticky-session RESUME instead of [`body`] (the history already carries Frame 2). The content
    /// AFTER `---resume---`. `None` = no marker → `body` is reused. Trusted.
    pub resume_body: Option<String>,
}

impl StatePrompt {
    /// Parse a `prompts/<id>.md` document (frontmatter + body) into a [`StatePrompt`].
    pub fn parse(id: impl Into<String>, text: &str) -> Result<Self> {
        let id = id.into();
        let (yaml, body) = split_frontmatter(text)?;
        let fm: StatePromptFrontmatter = serde_yaml::from_str(yaml).map_err(|e| {
            DackError::Stimulus(format!("state-prompt `{id}` frontmatter: {e}"))
        })?;
        let (system_frame, body, resume_body) = split_frames(body);
        Ok(StatePrompt {
            id,
            state: fm.state,
            mcp: fm.mcp,
            transitions: fm.transitions,
            model: fm.model,
            session: fm.session,
            reply_key: fm.reply_key,
            context: fm.context,
            system_frame,
            body,
            resume_body,
        })
    }

    /// The context-block knobs for this prompt (memory/runlog), or the all-on default.
    pub fn context(&self) -> ContextConfig {
        self.context.clone().unwrap_or_default()
    }

    /// The payload-item field(s) a baton's `reply_to` is matched against (the reply identifier). The
    /// soul declares it per-prompt (`reply_key`); unset = the platform-agnostic default
    /// `message_id`→`id` (covers telegram + twitter without config).
    pub fn reply_key_fields(&self) -> Vec<&str> {
        match &self.reply_key {
            Some(k) => vec![k.as_str()],
            None => vec!["message_id", "id"],
        }
    }

    /// The repo-relative file a state-prompt id resolves to (`prompts/<id>.md`).
    pub fn repo_path(id: &str) -> String {
        format!("prompts/{id}.md")
    }

    /// Whether `next` is in this prompt's declared transition set (the soul's half of the gate).
    pub fn permits_transition_to(&self, next: &str) -> bool {
        self.transitions.iter().any(|t| t == next)
    }
}

/// Split a state-prompt body on a `---resume---` marker line (a line that trims to exactly that token):
/// text BEFORE = the fresh-session body, text AFTER = the lean RESUME body. No marker → the whole body
/// and `None`. The marker shares the `---` of the frontmatter fence but `split_frontmatter` already
/// consumed the leading `---\n…\n---\n` and matches the fence as exactly `\n---\n`, so `---resume---`
/// in the body is never mistaken for it.
pub fn split_resume(body: &str) -> (String, Option<String>) {
    match body.lines().position(|l| l.trim() == "---resume---") {
        Some(i) => {
            let lines: Vec<&str> = body.lines().collect();
            let fresh = lines[..i].join("\n").trim().to_string();
            let resume = lines[i + 1..].join("\n").trim().to_string();
            (fresh, Some(resume))
        }
        None => (body.trim().to_string(), None),
    }
}

/// Split a state-prompt body into its THREE frames: `(system_frame, first_user, resume)`.
/// - `---task---` separates the **system frame** (stable rules → the system message, above the marker)
///   from the user-message frames (below). No `---task---` ⇒ empty system frame (back-compat).
/// - `---resume---` (within the part below `---task---`) separates the **first-user** teaching from the
///   lean **resume** cue, exactly as [`split_resume`].
pub fn split_frames(body: &str) -> (String, String, Option<String>) {
    let (system_frame, rest) = match body.lines().position(|l| l.trim() == "---task---") {
        Some(i) => {
            let lines: Vec<&str> = body.lines().collect();
            (lines[..i].join("\n").trim().to_string(), lines[i + 1..].join("\n"))
        }
        None => (String::new(), body.to_string()),
    };
    let (first_user, resume) = split_resume(&rest);
    (system_frame, first_user, resume)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_state_tier_mcp_and_transitions() {
        let text = "---\nstate: perceive\nmcp:\n  - cove-read\n  - { name: rootai, url: https://mcp.rootai.xyz }\ntransitions: [twitter/feed_reply, twitter/express_market_insights]\n---\nRead the room.\n";
        let sp = StatePrompt::parse("twitter/perceive_feed", text).unwrap();
        assert_eq!(sp.state, ConsciousnessState::Perceive);
        assert_eq!(sp.mcp.len(), 2);
        assert_eq!(sp.mcp[0], McpRef::Import("cove-read".into()));
        assert_eq!(
            sp.mcp[1],
            McpRef::Inline { name: "rootai".into(), url: "https://mcp.rootai.xyz".into() }
        );
        assert!(sp.permits_transition_to("twitter/feed_reply"));
        assert!(!sp.permits_transition_to("settle/cove"));
        assert_eq!(sp.body, "Read the room.");
        assert_eq!(StatePrompt::repo_path("twitter/perceive_feed"), "prompts/twitter/perceive_feed.md");
    }

    #[test]
    fn minimal_prompt_has_no_mcp_or_transitions() {
        let sp = StatePrompt::parse("settle", "---\nstate: settle\n---\nAct.").unwrap();
        assert_eq!(sp.state, ConsciousnessState::Settle);
        assert!(sp.mcp.is_empty());
        assert!(sp.transitions.is_empty());
    }

    #[test]
    fn context_config_parses_tag_key_and_runlog_views() {
        // Absent → defaults: memory on, no tag_key, environment 10, thread 10.
        let sp = StatePrompt::parse("p", "---\nstate: perceive\n---\nx").unwrap();
        assert!(sp.context.is_none());
        let c = sp.context();
        assert!(c.memory && !c.tag_key);
        assert_eq!((c.runlog.environment, c.runlog.thread), (10, 10));
        assert_eq!((c.runlog.thread_recent_lookback_secs, c.runlog.recent_notes), (3600, 20));
        assert!(c.runlog.dedup_notes, "dedup defaults on (chats collapse to latest-per-tag)");
        // Declared → tag_key on + runlog knobs; unset fields keep their defaults.
        let sp = StatePrompt::parse(
            "p",
            "---\nstate: perceive\ncontext: { tag_key: true, runlog: { environment: 40, thread: 20 } }\n---\nx",
        )
        .unwrap();
        let c = sp.context();
        assert!(c.memory && c.tag_key);
        assert_eq!((c.runlog.environment, c.runlog.thread), (40, 20));
        // Partial runlog → named field overrides, the others default.
        let sp = StatePrompt::parse("p", "---\nstate: perceive\ncontext: { runlog: { thread: 5 } }\n---\nx").unwrap();
        let c = sp.context();
        assert_eq!((c.runlog.environment, c.runlog.thread), (10, 5));
    }

    #[test]
    fn resume_marker_splits_body_into_fresh_and_lean() {
        // With a `---resume---` line: before = fresh body, after = lean resume body.
        let sp = StatePrompt::parse(
            "telegram/perceive",
            "---\nstate: perceive\n---\nFull teaching here.\n\n---resume---\nYou're resuming. Only new stuff.",
        )
        .unwrap();
        assert_eq!(sp.body, "Full teaching here.");
        assert_eq!(sp.resume_body.as_deref(), Some("You're resuming. Only new stuff."));

        // No marker: body is the whole thing, resume_body None (unchanged behavior).
        let sp = StatePrompt::parse("settle", "---\nstate: settle\n---\nAct.").unwrap();
        assert_eq!(sp.body, "Act.");
        assert_eq!(sp.resume_body, None);
    }

    #[test]
    fn three_frame_split_separates_system_first_user_and_resume() {
        // `---task---` splits the SYSTEM frame (stable rules → system msg) from the user-message frames;
        // `---resume---` then splits the first-user teaching from the lean resume cue.
        let sp = StatePrompt::parse(
            "telegram/express",
            "---\nstate: express\n---\nExpress: reply-only; one outward action.\n---task---\nThe full how-to and voice.\n---resume---\nContinuing this thread — reply in voice.",
        )
        .unwrap();
        assert_eq!(sp.system_frame, "Express: reply-only; one outward action.");
        assert_eq!(sp.body, "The full how-to and voice.");
        assert_eq!(sp.resume_body.as_deref(), Some("Continuing this thread — reply in voice."));

        // No `---task---` ⇒ empty system frame, the rest is the (user-message) body — back-compat.
        let sp = StatePrompt::parse("p", "---\nstate: perceive\n---\nJust the body.").unwrap();
        assert_eq!(sp.system_frame, "");
        assert_eq!(sp.body, "Just the body.");
        assert_eq!(sp.resume_body, None);
    }

    #[test]
    fn reply_key_fields_default_and_declared() {
        // Declared → that one field; the model's `reply_to` is matched against it.
        let sp = StatePrompt::parse(
            "telegram/perceive",
            "---\nstate: perceive\nreply_key: message_id\ntransitions: [telegram/express]\n---\nRead.",
        )
        .unwrap();
        assert_eq!(sp.reply_key_fields(), vec!["message_id"]);
        // Unset → the platform-agnostic default (message_id, then id).
        let sp = StatePrompt::parse("settle", "---\nstate: settle\n---\nAct.").unwrap();
        assert_eq!(sp.reply_key_fields(), vec!["message_id", "id"]);
    }
}
