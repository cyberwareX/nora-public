# Core concepts

dack-engine runs an autonomous agent as a long-lived actor. The model is sovereign over *what to
think and say*; the harness is sovereign over *what is reachable*. Two mechanisms enforce that split:
the **consciousness states** (a reversibility ladder) and the **trust lattice** (an information-flow /
taint model). A request the model makes only executes if both allow it.

## The agent loop

Work arrives as a **stimulus** (a Telegram message, a poll result, a cron tick, an operator
instruction). The harness queues it, then runs a **cycle**: one or more model invocations, each in a
**consciousness state**, until the cycle transitions to a terminal state or stops.

Each invocation is driven by a **state-prompt** — a markdown file in the soul's `prompts/` tree with
YAML frontmatter declaring its `state`, the MCP capabilities it requests, and which prompts it may
transition to. A cycle walks from one state-prompt to the next, picking exactly one allowed transition
per step (or terminating). See [authoring souls](authoring-souls.md).

## The four consciousness states

The states form a one-way **reversibility ladder** — each rung admits strictly more dangerous
actions, and a cycle can only climb if it stays clean (see trust below).

| State | Admits | Typical use |
|---|---|---|
| **Perceive** | reads only (no writes, no external effects) | digest the stimulus, gather context, decide intent |
| **Express** | reversible effects + memory writes | reply, post, send — anything undoable |
| **Settle** | irreversible authority | a trade, a transfer, an on-chain vote |
| **Reflect** | self-modification (writes to the soul) | edit prompts, memory, skills, sensors |

The harness classifies every tool the model calls into a **tool class**, and each class is permitted
only in certain states:

- **Read** — file reads, search, web fetch, MCP resource reads. Safe in every state.
- **FileWrite** — the single Write/Edit tool. The *path* decides what it touches: `memory/`
  (long-term memory) is writable in Express/Settle but only by a sufficiently-trusted cycle (see
  [Memory](#memory-short-term-vs-long-term) below); the soul's own directories (`prompts/`, `skills/`,
  sensors) are writable only in Reflect. The harness-authored `runlogs/` are never agent-writable.
- **Post** — a reversible capability MCP tool (e.g. `mcp__twitter__post`). Express only.
- **SettleTx** — an irreversible capability MCP tool. Settle only.
- **Shell** — raw Bash/REPL. Denied in every consciousness state (it would bypass the path gate). Real
  capabilities are exposed as MCP tools with cleanly gateable names instead. (A delegated worker is the
  exception — it runs in its own sandbox; see [workers](workers.md).)

This classification is **the wall**: every tool the model wants to call is relayed to the harness,
which decides allow/deny by class and current state before the call executes.

## The trust lattice (taint)

States bound *what kind* of action is reachable. The trust lattice bounds *how far a particular cycle*
may climb, based on what data it has touched.

Every cycle carries a **trust tier**. The tiers are an ordered list (low→high); the default lattice is:

```
public  →  reaches Express   (tweets, 3rd-party data: reversible only)
self    →  reaches Reflect   (the operator's own data: may even self-edit)
operator_signed → reaches Reflect (a DID-signed operator instruction)
```

Each tier declares the highest state it **reaches**. A cycle may only walk to a state its current trust
reaches. You can define custom tiers (e.g. an `org` tier between `public` and `self` that reaches
`settle`) — the three well-known names must always be present.

Two rules move a cycle's trust:

1. **Seeding (provenance).** A cycle starts at a tier derived from its *source*: a webhook path's
   configured tier, an operator-signed instruction (`operator_signed`), a signed sensor script's tier,
   or `public` by default. Provenance seeds trust — never agent-asserted metadata.
2. **Taint (monotonic descent).** Calling a tool degrades the cycle's trust to the **meet** (the lower
   rank) of its current trust and the tool's declared `trust` label. Trust only ever ratchets *down*
   within a cycle.

The consequence is the central safety property: **a cycle that touches public data can never reach an
irreversible or self-modifying action.** If the agent reads a tweet (a `public` capability) while
holding `self` trust, it degrades to `public` and loses the ability to trade or self-edit for the rest
of that cycle — automatically, with no rule to write.

There is also an **authorization axis**, orthogonal to the state and the taint: a capability can carry
`min_trust`, and is only assembled for a cycle whose current trust ranks at or above it. A capability
gated `min_trust: org` is unreachable from a `public` cycle, and is lost mid-cycle if an `org` cycle
degrades. Net: **state = what kind of action · reaches = how far · min_trust = who's allowed.**

## The firebreak

When a cycle moves from Perceive into a later state, the model does **not** carry the raw stimulus
forward. Perceive emits a digested **gist** — its own summary of intent — and the next state runs fresh
with only that gist plus harness-trusted provenance annotations. Untrusted world text never crosses
into the acting states verbatim, so a prompt injection in an incoming message cannot directly drive a
post, a trade, or a self-edit; at most it can shape a gist that the model itself authored.

The same firebreak applies to delegated work: a worker's result re-enters the agent as a new,
untrusted stimulus (`public`), to be judged like any other input — never as a trusted continuation.

## Memory: short-term vs long-term

The agent has two memories, mirroring a mind:

- **Short-term — the runlog.** The harness writes one append-only record per cycle to `runlogs/<date>.md`
  (a separate, private store). It ages off (a configurable retention window, e.g. ~10 days). The agent
  never reads or writes these files directly — it reaches them only through **recall** tools (so a
  fresh or context-evicted session can reconstruct what just happened). Alongside the entries is a
  **tag-notes catalogue**: short "sticky notes" the agent leaves about a conversation as it goes
  (`{tag, note}`), which the harness stamps with the writing cycle's **trust** (unfakeable provenance —
  a public chat's note is `public`, a trusted cycle's is `self`).

- **Long-term — `memory/`.** Durable knowledge: who people are, lasting context, the agent's own notes
  to itself. It is **trust-gated**: only a cycle at or above `memory_write_min_trust` (default `org`)
  may write `memory/`. An untrusted in-run cycle therefore cannot launder a note into long-term memory;
  it can only leave a short-term, trust-stamped tag-note.

Two recall surfaces let the model navigate its own history, and the *choice between them is a trust
decision the model makes*:

- **`recall`** returns the verbatim transcript (incl. others' words). Reading it taints the cycle
  `public` — raw untrusted text crossing back in.
- **`recall-self`** returns only the agent's *own* post-firebreak output (its thoughts + what it said,
  plus the tag-notes), never the raw incoming text. Because that's the trusted side of the firebreak,
  reading it does **not** taint the cycle — a trusted job can review its past and still act.

**Digest jobs** bridge the two: a scheduled, self-trust cycle reads short-term memory via `recall-self`
(staying clean) and consolidates the durable parts into long-term `memory/`. This is the only path by
which conversation activity becomes long-term memory, which keeps the trust boundary intact and makes
`memory/` a single-writer-class resource.

## Where the boundaries live

The model is given the soul prompts as its system prompt and asked to reason and act. None of the
boundaries above depend on the model cooperating:

- The **wall** denies out-of-state tool classes regardless of what the model attempts.
- The **taint ceiling** caps reachable states regardless of what the model claims its trust is.
- The **path gate** confines file writes regardless of which write tool the model uses.
- The **integrity tripwire** reverts any soul change made outside a Reflect cycle.

The model is sovereign over judgment and voice; the harness is the deterministic substrate that makes
the dangerous surface unreachable when it should be.
