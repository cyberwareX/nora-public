# Authoring a soul

The **soul** is the agent's durable identity and behavior — a git repository, separate from the engine,
that the harness reads and the agent (in Reflect) may edit. Clone the public template
(`git clone https://github.com/obraztsov/dack-soul.git my-soul`) and point `config.soul_repo` at your copy.

```
soul/
├── SOUL.md            # who the agent is — the top of every system prompt
├── prompts/           # state-prompts: one markdown file per step of a cycle
├── stimuli/           # duties: what wakes the agent (cron / webhook + sensor)
├── agents/            # worker definitions the agent may delegate to
├── memory/            # long-term memory (durable notes; trust-gated writes)
├── skills/            # reference material / scripts the agent can read
└── runlogs/           # short-term memory: the append-only log the harness writes
```

## `SOUL.md`

Free-form markdown describing the agent — its identity, voice, values, hard boundaries. It is prepended
to every state-prompt as the system prompt, so it is the one place that shapes *all* of the agent's
behavior. Keep boundaries explicit here (what it will never do, who its operator is by exact handle/id,
not display name).

## State-prompts (`prompts/`)

Each step of a cycle runs a state-prompt: a markdown file whose **body** is the instruction for that
step and whose **frontmatter** declares its place in the machine. Prompts can be nested
(`prompts/twitter/perceive.md`) and referenced by their path-without-extension (`twitter/perceive`).

```markdown
---
state: perceive                 # which consciousness state this runs in
mcp: [cove-read, twitter-read]  # capabilities requested (admitted by tier_policy)
transitions: [express]          # the prompts it may walk to next (pick one, or stop)
# model: your-model-id          # optional, only if the tier allows override
# session: { sticky: true, key: [thread_id] }   # optional resume-by-thread
---
You are in Perceive. You are read-only. Digest the incoming stimulus through your standing
duty, decide what (if anything) to do, and either transition to `express` to act, or stop.

Return a structured result:
- thought: your reasoning (logged, never published)
- batons: [ { to_prompt, gist, priority? } ]   # your fan-out: 0, 1, or several branches
- spawn: { agent, brief } | null               # optional: delegate a job to a worker
- tag_notes: [ { tag, note } ]                  # optional short-term "sticky notes" (below)
```

`tag_notes` are short notes the agent leaves about a thread as it goes — `{ tag, note }`, e.g.
`{ tag: "chat:42", note: "asked me to track the launch" }`. They are part of **short-term** memory: the
harness appends them to the runlog catalogue and **stamps each with the writing cycle's trust** (the agent
does not — and cannot — set it), so a public chat's note is provably `public`. Any state may leave one,
including Perceive. The agent retrieves them later through the recall tools; they never become long-term
memory except through a digest.

`batons` is the fan-out: **one** element takes a single next step (the common case), **several** do
several things at once — each its own digested gist + destination, each gated independently by the
cycle's trust ceiling — and `[]` stops. A branch marked `priority: low` is **deferred** to the queue so
a higher-priority stimulus can be handled first; everything else runs immediately. (The legacy single
`transition: { to_prompt }` + `proposal: { gist }` is still accepted and folds to one branch.)

Frontmatter fields:

| Field | Meaning |
|---|---|
| `state` | `perceive` / `express` / `settle` / `reflect` — bounds the tool classes allowed. |
| `mcp` | Capabilities to request. Each is admitted only if `tier_policy.<state>.import` lists it (the handshake). |
| `transitions` | The allowed next prompt ids; the run picks exactly one (or terminates). The target's state is ceiling-checked against the cycle's trust before it opens. |
| `model` | Optional per-prompt model, honored only where `tier_policy.<state>.allow_model_override` is set. |
| `session` | Optional sticky session: `{ sticky: true, key: [thread_id] }` resumes the same engine session across stimuli that share the key (cheaper context, conversational memory). A different prompt or trust tier is always a different session — the firebreak holds. |

The model always returns a structured result (a thought, an optional proposal/gist, an optional
`spawn` for delegation, and a transition). The harness reads the transition and walks the machine.

## Duties (`stimuli/`)

A duty is what *wakes* the agent. Each is a directory with a `STIMULUS.md`:

```markdown
---
id: twitter-mentions
trigger: { type: cron, schedule: "*/5 * * * *" }   # or { type: webhook, path: /telegram/op }
sensor: ./scripts/fetch_mentions.py                 # optional: a fetch step
secrets: [x]                                        # provider tokens for the sensor
emits: { type: mention }
entry: twitter/perceive_mention                     # the state-prompt the cycle opens at
directive_tier: self                                # trust of THIS duty's standing directive
priority: normal
coalesce: { mode: batch, window_sec: 60 }           # optional: fold a burst into one wake
cursor: { field: id, env: SINCE_ID }                # optional: cross-poll dedup watermark
---
Standing directive (trusted): the durable instruction for this duty — what the agent is here to do.
```

| Field | Meaning |
|---|---|
| `id` | Stable duty id. |
| `trigger` | `{ type: cron, schedule }` or `{ type: webhook, path }`. |
| `sensor` | Optional script the harness runs (read-scoped) to fetch new items. Omit for a webhook (the POST body is the payload) or a pure-cron self-prompt. |
| `secrets` | Provider names whose token env is injected into the sensor. |
| `emits` | `{ type }` — the stimulus type produced. |
| `entry` | The state-prompt id a cycle opens at. |
| `directive_tier` | The trust of this duty's *standing directive* (the trusted `.md` body). The *payload's* trust is seeded separately from the source (signed-script / webhook-path / `self` for pure-cron). |
| `priority` | `urgent` / `high` / `normal` / `low` — queue ordering (urgent first; low is sheddable under load). |
| `dispatch_window` | Optional `"HH:MM-HH:MM"` (UTC, may cross midnight). A stimulus arriving outside the window waits until it next opens — e.g. handle a noisy public group only at night. |
| `coalesce` | Optional folding (below). |
| `cursor` | Optional `{ field, env }` cross-poll watermark, so a poller never re-surfaces a handled item. |

### Coalescing

A busy source can fold a burst into one wake instead of one cycle per item:

```yaml
coalesce: { mode: batch, window_sec: 60 }
```

- `mode`: `batch` (fold all items in the window into one wake, presented as a list), `latest` (keep
  only the newest), or `none`.
- `window_sec`: the debounce window — messages keyed the same accumulate for this long, then fire as
  one wake. A short window stays responsive; a long window batches hard (good for a noisy public group).

Coalescing keys on the candidate's dedup key (e.g. a chat id), so each conversation batches separately.

## Workers (`agents/`)

Definitions for delegated subagents the agent may spawn from an act state. See
[workers](workers.md).

## Memory & skills

The agent has **two** memories (see [concepts](concepts.md#memory-short-term-vs-long-term)):

- **Short-term — the runlog.** Append-only, retention-bounded (`runlog_retention_days`), harness-written;
  the agent never reads or writes the files. It reaches its own history through **recall** MCP servers,
  which a state-prompt requests like any capability and which the soul template wires up:
  - `recall` returns the verbatim transcript (others' words included) and **taints** the cycle `public`.
  - `recall-self` returns only the agent's own past thoughts, gists, and tag-notes — the trusted side of
    the firebreak — so it does **not** taint. A cycle that needs to stay trusted (to write memory or act)
    uses this one.
  - Both also expose discovery tools (list days, list tags, search) so a fresh session can reorient.

- **Long-term — `memory/`.** The agent's durable notes (an `INDEX.md` the agent can scan helps it find
  them). Writing `memory/` is **trust-gated**: only a cycle at or above `memory_write_min_trust` (default
  `org`) may write it — an untrusted in-run cycle can leave a short-term tag-note but cannot launder into
  long-term memory. It is still soul-protected against `skills/`/prompts edits outside Reflect.

`skills/` holds reference material and read-only scripts the agent can consult — part of the
firebreak-protected soul, writable only in Reflect.

### The digest pattern (short-term → long-term)

The only path from conversation activity into long-term memory is a **digest**: a scheduled cron duty that
runs at `self` trust (`directive_tier: self`, pure-cron so no untrusted payload) with a two-prompt cycle —
a Perceive step that reads short-term memory via `recall-self` (staying clean), then a distill step that
consolidates the durable parts into `memory/` and optionally surfaces suggestions to the operator over
`telegram-send`. The template ships `activity-digest` and `twitter-digest` as working examples. Because the
perceive side never touches `recall` (the tainting one), the whole cycle keeps the trust needed to write
memory — the trust boundary stays intact and `memory/` stays single-writer-class.

## Self-modification (Reflect)

In a Reflect cycle the agent may write its own `prompts/`, `skills/`, sensors, and memory — that is how
it learns and adapts. Every other cycle is blocked from those paths by the path gate, and the integrity
tripwire reverts any soul change made outside Reflect. To hand-edit the soul yourself without the
tripwire reverting it, run `dack reconcile` (see [operations](operations.md)).
