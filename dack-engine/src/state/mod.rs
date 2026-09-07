//! Consciousness states — the security model, hardcoded.
//!
//! A consciousness state = an OpenClaude invocation with a fixed
//! **(system prompt + allowed-tool set + model)** triple plus a set of writable dirs.
//! The harness owns these triples; **the agent cannot widen them**. States are
//! hardcoded *because they are the security model*; only the *routing between them*
//! is operator config (the product-iteration surface).
//!
//! The cut is by **reversibility**, not by "does it write memory":
//!   - Perceive  — read-only, can't act (digests untrusted input → a typed proposal)
//!   - Express   — reversible writes (post/reply + memory-append)
//!   - Settle    — irreversible (EVM tx / vote) — THE wall; unreachable in v1
//!   - Reflect   — modifies soul/skills/stimuli/prompts; harness-entered & -exited only

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsciousnessState {
    Perceive,
    Express,
    Settle,
    Reflect,
}

/// Per-state model routing — the primary inference-cost lever.
/// Maps onto OpenClaude `agentRouting`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// MiMo / Ollama-local — cheap/quiet runs (Perceive).
    CheapLocal,
    /// Mixed local+frontier (Express).
    Mixed,
    /// Frontier model — hard runs (Settle, Reflect).
    Frontier,
}

/// Tool *classes* the `action_required` responder gates on. These are
/// coarse capability buckets, not the full OpenClaude tool list — the responder maps
/// a concrete tool call to its class, then checks class membership for the state.
/// Capability buckets the `action_required` responder gates on. Grounded to
/// OpenClaude's *actual* tool surface (audited 0.15.0): the responder receives a raw
/// `(tool_name, input)` and maps it to one of these via [`crate::runtime::classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolClass {
    /// Pure LOCAL reads — FileRead, Grep, Glob, LSP, MCP resource reads, ToolSearch,
    /// Task{Get,List,Output}. Safe in every state. (NETWORK reads are the separate `Net` class.)
    Read,
    /// Network egress — `WebFetch` (fetch an arbitrary URL) / `WebSearch`. Distinct from a local
    /// `Read`: it reaches the OUTSIDE network, so it is a callback / SSRF / **exfiltration** surface —
    /// a prompt-injected cycle could be steered to fetch `attacker/?leak=<secret>` or probe an internal
    /// address, and the request LEAVES regardless of the firebreak. Scoped to **Perceive only**
    /// (read-only context-gathering) — NOT Express/Settle/Reflect, so an injected ACT cycle can't reach
    /// the network. The sandboxed worker keeps it (research in a throwaway workspace).
    Net,
    /// File write/edit — the single OpenClaude Write/Edit/NotebookEdit tool. **The path,
    /// not the tool, decides memory-vs-soul**: gated by `writable_dirs` (memory/ in
    /// Express/Settle; soul dirs in Reflect). One class; the per-path gate does the rest.
    FileWrite,
    /// Reversible external effect — a *capability MCP tool* (e.g. `mcp__twitter__post`).
    /// Capabilities are MCP tools, NOT bash skill-scripts, precisely so they have clean
    /// gateable names (see [`Shell`](ToolClass::Shell) and VERIFICATION.md).
    Post,
    /// Irreversible authority — a *configured settle MCP tool* (e.g. `mcp__bankr__send`,
    /// `mcp__dac__vote`). Runs ONLY in Settle, reachable only by an uncontaminated cycle (taint).
    SettleTx,
    /// Arbitrary execution — Bash / PowerShell / REPL. **Denied in every v1 state.** Raw
    /// shell bypasses `writable_dirs` path-gating (a bash `>` can write soul dirs and
    /// defeat the Reflect-only self-modification invariant), so it is never in scope; real
    /// capabilities are exposed as MCP tools instead.
    Shell,
    /// Anything else — Agent, Skill, misc MCP, unknown tools. **Default-deny** unless a
    /// state explicitly scopes it. Unknown ⇒ denied (fail-closed).
    Other,
}

/// The fixed capability scope for a state. The set is closed at compile time; the
/// agent has no path to extend it.
#[derive(Debug, Clone)]
pub struct ToolScope {
    pub allowed: Vec<ToolClass>,
}

impl ToolScope {
    pub fn allows(&self, class: ToolClass) -> bool {
        self.allowed.contains(&class)
    }
}

/// The full (prompt + tools + model + writable dirs) spec for a state — the  table
/// as data. `writable_dirs` is the write-gating from: a prefix allow-list the
/// responder checks for any file-write target. `runlogs/` is in NO state's list —
/// the harness authors the record.
#[derive(Debug, Clone)]
pub struct StateSpec {
    pub state: ConsciousnessState,
    /// Path within the soul repo, e.g. "prompts/perceive.md".
    pub prompt_path: &'static str,
    pub model: ModelTier,
    pub tool_scope: ToolScope,
    /// Repo path prefixes this state may write. Empty = read-only.
    pub writable_dirs: Vec<&'static str>,
}

/// The table, as code. This is the single source of truth for write-gating;
/// the responder consults it and is never trusted to the agent.
pub fn default_spec(state: ConsciousnessState) -> StateSpec {
    use ConsciousnessState::*;
    use ToolClass::*;
    match state {
        Perceive => StateSpec {
            state,
            prompt_path: "prompts/perceive.md",
            model: ModelTier::CheapLocal,
            // Read-only: holds NO write tools at all, so raw-stimulus processing
            // cannot mutate state. Net (WebFetch/WebSearch) is admitted HERE ONLY — context-gathering
            // is a Perceive job, and a fetch here can't be chained to an outward act (that's Express).
            tool_scope: ToolScope { allowed: vec![Read, Net] },
            writable_dirs: vec![],
        },
        Express => StateSpec {
            state,
            prompt_path: "prompts/express.md",
            model: ModelTier::Mixed,
            // FileWrite is gated to memory/ by writable_dirs; Post is the capability MCP
            // tool. No Shell — bash would bypass the path gate.
            tool_scope: ToolScope {
                allowed: vec![Read, FileWrite, Post],
            },
            writable_dirs: vec!["memory/"],
        },
        Settle => StateSpec {
            state,
            prompt_path: "prompts/settle.md",
            model: ModelTier::Frontier,
            tool_scope: ToolScope {
                allowed: vec![Read, FileWrite, SettleTx],
            },
            // Writes memory like Express; the irreversible part is the SettleTx class,
            // the irreversible part is the SettleTx class; Settle reachability is taint-gated.
            writable_dirs: vec!["memory/"],
        },
        Reflect => StateSpec {
            state,
            prompt_path: "prompts/reflect.md",
            model: ModelTier::Frontier,
            // The ONLY state whose writable_dirs include the soul dirs — same FileWrite
            // class as Express, but a wider path allow-list.
            tool_scope: ToolScope {
                allowed: vec![Read, FileWrite],
            },
            writable_dirs: vec!["skills/", "stimuli/", "prompts/", "SOUL.md", "memory/", "agents/"],
        },
    }
}

/// The capability spec for a **worker** — keyless sandboxed compute the duck wields, NOT
/// a consciousness state. Reuses [`StateSpec`] (the responder's shape); `state` is cosmetic here (a
/// worker carries no Post/Settle MCP tools — `mcp_servers={}` — so the scope never emits a
/// state-named deny). The harness runs it in its own `/workspace` and sets the responder's
/// relativize root to that workspace, so the worker may Read/Write/Edit/Bash there and spawn SYNC
/// sub-helpers (`Other` = the SDK `Task` tool) — but has **no Post/SettleTx** (no outward authority)
/// and **cannot write the soul repo** (soul paths don't relativize into the workspace → denied; the
/// soul-integrity tripwire is the backstop for any Bash write).
pub fn worker_spec() -> StateSpec {
    use ToolClass::*;
    StateSpec {
        state: ConsciousnessState::Express, // cosmetic; a worker is not a consciousness state (see doc)
        prompt_path: "",
        model: ModelTier::Mixed,
        tool_scope: ToolScope {
            allowed: vec![Read, Net, FileWrite, Shell, Other],
        },
        // "" = anything under the workspace (the responder's relativize root); the soul stays out.
        writable_dirs: vec![""],
    }
}

/// State-transition rules. Encodes three invariants:
///   1. Perceive may open Express or Settle (via the Baton). Perceive proposes; the
///      harness opens the next state.
///   2. **Reflect is harness-entered only** — no state transitions *into* Reflect.
///      The agent cannot choose to reflect; an injected agent therefore cannot trigger
///      an immediate reflection to rewrite its own skills mid-attack. Self-modification
///      is real but rate-limited by the harness clock.
///   3. **Reflect is harness-exited only** — Reflect transitions into no other state.
///      It influences the future only indirectly, by writing `memory/` and `stimuli/`.
pub fn allowed_transition(from: ConsciousnessState, _to: ConsciousnessState) -> bool {
    use ConsciousnessState::*;
    // The ONE remaining structural constraint (invariant 3): **Reflect exits to nothing**. Reflect
    // is now REACHABLE via transition — but only from an UNCONTAMINATED cycle whose trust
    // tier `reaches: reflect`, and rate-limited by the harness clock (the injection-resistance the
    // old into-Reflect ban gave is now the taint guarantee + the rate-limit; see `harness::dispatch`
    // and). How far any chain walks is bounded by the taint-derived ceiling.
    from != Reflect
}

/// Reachability rank for the route-ceiling clamp: `Perceive < Express < Settle`. `Reflect` ranks
/// above only so the comparison is total; it is harness-only and never a ceiling/transition target.
pub fn reach_rank(s: ConsciousnessState) -> u8 {
    use ConsciousnessState::*;
    match s {
        Perceive => 0,
        Express => 1,
        Settle => 2,
        Reflect => 3,
    }
}

/// Whether opening `state` is within the operator-declared route `ceiling`. The ceiling is
/// the highest consciousness tier a payload-class may walk to; the operator sets it per route
/// (`ceiling: settle` is the authority that lets a self-tier trade duty reach Settle — what the old
/// `EntryState::PerceiveThenSettle` force did). A `settle` ceiling is additionally guarded at config
/// load to non-public routes (`DackConfig::validate_capabilities`).
pub fn within_ceiling(state: ConsciousnessState, ceiling: ConsciousnessState) -> bool {
    reach_rank(state) <= reach_rank(ceiling)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConsciousnessState::*;

    #[test]
    fn perceive_is_read_only() {
        let spec = default_spec(Perceive);
        assert!(spec.tool_scope.allows(ToolClass::Read));
        // A file-write in Perceive is impossible by construction.
        assert!(!spec.tool_scope.allows(ToolClass::FileWrite));
        assert!(!spec.tool_scope.allows(ToolClass::Post));
        // Bash is never in scope anywhere — it would bypass path-gating.
        assert!(!spec.tool_scope.allows(ToolClass::Shell));
        assert!(spec.writable_dirs.is_empty());
    }

    #[test]
    fn only_reflect_writes_soul_dirs() {
        // Soul-write is now path-gated FileWrite: the class is shared, the PATH allow-list
        // is what differs — only Reflect lists the soul dirs.
        for dir in ["skills/", "stimuli/", "prompts/", "SOUL.md"] {
            assert!(
                default_spec(Reflect).writable_dirs.contains(&dir),
                "Reflect must be able to write {dir}"
            );
            for s in [Perceive, Express, Settle] {
                assert!(
                    !default_spec(s).writable_dirs.contains(&dir),
                    "{s:?} must not be able to write {dir}"
                );
            }
        }
    }

    #[test]
    fn bash_is_denied_in_every_state() {
        for s in [Perceive, Express, Settle, Reflect] {
            assert!(
                !default_spec(s).tool_scope.allows(ToolClass::Shell),
                "{s:?} must not have raw Shell in scope (it bypasses writable_dirs)"
            );
        }
    }

    #[test]
    fn network_egress_is_perceive_only() {
        // Net (WebFetch/WebSearch) = context-gathering, admitted ONLY in Perceive — an ACT/self-mod
        // cycle (Express/Settle/Reflect) must never reach the network (exfil/SSRF/callback surface).
        assert!(default_spec(Perceive).tool_scope.allows(ToolClass::Net), "Perceive gathers context");
        for s in [Express, Settle, Reflect] {
            assert!(
                !default_spec(s).tool_scope.allows(ToolClass::Net),
                "{s:?} must NOT reach the network (WebFetch/WebSearch)"
            );
        }
        // The sandboxed worker keeps network for research.
        assert!(super::worker_spec().tool_scope.allows(ToolClass::Net), "worker may research online");
    }

    #[test]
    fn no_state_may_write_runlogs() {
        for s in [Perceive, Express, Settle, Reflect] {
            assert!(
                !default_spec(s).writable_dirs.iter().any(|d| d.starts_with("runlogs")),
                "{s:?} must not be able to write runlogs/"
            );
        }
    }

    #[test]
    fn transition_invariants() {
        // Reflect EXITS to nothing (the one remaining structural ban).
        for s in [Perceive, Express, Settle, Reflect] {
            assert!(!allowed_transition(Reflect, s), "Reflect → {s:?} must be forbidden");
        }
        // Reflect is now REACHABLE via transition — gated by the taint ceiling + the
        // rate-limit, NOT a structural ban. Every non-Reflect→* edge is structurally allowed.
        assert!(allowed_transition(Perceive, Reflect));
        assert!(allowed_transition(Perceive, Express));
        assert!(allowed_transition(Express, Settle));
        assert!(allowed_transition(Express, Express));
    }

    #[test]
    fn ceiling_clamps_how_far_a_chain_walks() {
        // An Express ceiling admits Perceive + Express, never the irreversible Settle.
        assert!(within_ceiling(Perceive, Express));
        assert!(within_ceiling(Express, Express));
        assert!(!within_ceiling(Settle, Express));
        // A Settle ceiling admits the whole reversibility ladder up to Settle.
        for s in [Perceive, Express, Settle] {
            assert!(within_ceiling(s, Settle), "{s:?} must be within a Settle ceiling");
        }
    }
}
