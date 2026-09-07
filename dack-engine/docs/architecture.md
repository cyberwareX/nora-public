# Architecture

dack-engine is a single Rust crate that runs one autonomous agent as a durable actor. It splits into
two halves: a **dumb, deterministic ingestion layer** that turns events into queued stimuli, and a
**consciousness loop** that pops the queue and drives the model through the wall. The model itself runs
out-of-process behind a thin TypeScript bridge.

```
 sources                ingestion (no reasoning)              cognition
 ───────                ──────────────────────                ─────────
 cron ──┐
 webhook├─► FiredTrigger ─► sensor ─► bus ─► SQLite queue ─► consciousness loop ─► the wall
 (poll) ┘                  (normalize, coalesce, seed trust)   (single-flight)        │
                                                                     │                │ allow/deny
 modules (long-running channel adapters) ─► webhook ─────────┘       ▼                ▼
                                                              runtime bridge ◄──► model gateway
                                                              (bun + OpenClaude SDK)
                                                                     │
                                                              soul repo (prompts, memory,
                                                              stimuli, skills) — signed commits
```

## Ingestion layer

The half of the harness that touches attacker-influenced bytes. Everything here is **data**, never an
instruction; the firebreak sits downstream at Perceive.

- **Sources** (`src/sources/`) feed a single `FiredTrigger` channel. A **cron wheel** fires scheduled
  duties; a **webhook listener** (localhost-only, `webhook_addr`) accepts POSTs from local adapters.
- **Sensors** (`src/sensor/`) are the duty's optional fetch step — a script the harness runs with a
  read-scoped env (plus any provider-materialized secret token) to pull new items. A webhook duty
  needs no sensor: the POSTed body is already the payload.
- **The bus** (`src/bus/`) normalizes each candidate into a `Stimulus`, applies the duty's **coalesce**
  policy (fold a burst into one wake, optionally debounced), seeds its **trust tier** from the source
  (provenance), and enqueues it.
- **The queue** (`src/queue/`) is embedded SQLite — durable across restarts, single source of pending
  work. It supports priority ordering, exact-once leasing, batch coalescing, and a per-row debounce
  gate.

## Consciousness loop

`src/harness/` pops one stimulus at a time (single-flight — the agent never runs two cycles at once)
and dispatches it:

1. Resolve the stimulus's **entry state-prompt** and open a cycle there.
2. Assemble the invocation: the soul prompts as the system prompt, the digested context as user
   content, and the **MCP capabilities** the state-prompt requests ∩ what `tier_policy` admits ∩ what
   the cycle's current trust permits.
3. Invoke the runtime. Every tool the model calls is relayed back through **the wall**
   (`src/runtime/action_required.rs` + `src/state/`), which decides allow/deny by tool class, current
   state, and the path gate, and records each call in the runlog.
4. The model returns a structured output (a thought, an intent, an optional transition). The harness
   walks to the chosen transition if the cycle's trust ceiling reaches it, else terminates.

Harness-owned background tasks run alongside the loop: the **Reflect scheduler** (enqueues the nightly
self-modification run), the **modules supervisor** (keeps channel adapters alive), and the **stimuli
watcher** (hot-reloads the soul's duties).

## The runtime bridge

The model runs out-of-process. `src/runtime/` defines a `RuntimeClient` trait; the wired
implementation spawns `bun run bridge.ts` (`openclaude-bridge/`), which drives the OpenClaude SDK and
speaks a small JSON protocol over stdio: one invocation in, a stream of permission events out (each
answered by the wall), and a final structured result. A pipe to a child is more confined than a
localhost socket — nothing binds, nothing is impersonable.

The runtime is an **extensibility seam**: `runtime.engine` chooses the agent runtime and
`runtime.connector` chooses how it reaches models (an OpenAI-compatible gateway today). Swapping either
is a config edit, not a code change. See [configuration](configuration.md#runtime).

## The sandbox seam

Subprocesses (sensors, the bridge, modules, workers) are spawned through a `Sandbox` trait
(`src/sandbox/`). The default `HostSandbox` runs them directly; a `DockerSandbox` maps an isolation
policy to `docker run` flags. This is how delegated **workers** get OS-level isolation
(see [workers](workers.md)) and where future sensor/container hardening slots in.

## The soul repo

The agent's durable identity is a git repository — the **soul** — separate from this source tree
(cloned from a public template into its own repo). It holds the agent's prompts, memory, stimuli, skills,
and runlogs. The harness commits to it as the agent's own DID and pushes it to one or more git remotes —
a plain `git push` to GitHub by default, optionally a best-effort signed Gitlawb mirror — so the agent's
history is attributable to it. An **integrity tripwire**
reverts any change to the soul made outside a Reflect cycle, so self-modification stays gated.

See [authoring souls](authoring-souls.md) for the bundle layout and [operations](operations.md) for the
signing/identity setup.

## Memory & digests

The agent has two memories (see [concepts](concepts.md#memory-short-term-vs-long-term) for the model):

- **Short-term** is the **runlog** (`src/runlog/`) — one append-only record per cycle, plus a tag-notes
  catalogue, kept in the soul under `runlogs/` and retention-bounded (`runlog_retention_days`). The agent
  never reads those files directly; it reaches them through two **recall** MCP servers — `recall` (verbatim
  transcript, taints the cycle `public`) and `recall-self` (only the agent's own post-firebreak thoughts +
  tag-notes, does **not** taint). The recall surface (`openclaude-bridge/runlog-recall*.ts`) also exposes
  discovery tools (list days, list tags, search) so a fresh or context-evicted session can reorient.
- **Long-term** is `memory/` in the soul — durable knowledge, **trust-gated** to `memory_write_min_trust`
  (default `org`) at the path gate.

The bridge between them is a **digest** — a scheduled, `self`-trust cron cycle (e.g. `activity-digest`,
`twitter-digest`) that perceives short-term memory via `recall-self` (staying clean) and distills the
durable parts into `memory/`, optionally surfacing suggestions to the operator over Telegram. Digests are
the *only* path by which conversation activity becomes long-term memory, keeping `memory/` single-writer.

## Observability

Operational logging is structured (`tracing` → stdout, configured by `log`): each cycle is a correlation
span, every model call emits a latency + token-usage line, and lifecycle/scheduling/error events stream as
pretty text on a TTY or JSON when containerized. This is distinct from the runlog (the agent's memory); see
[operations](operations.md#observing-a-run).

## Module map

| Module | Responsibility |
|---|---|
| `src/sources/` | cron wheel + webhook listener → one `FiredTrigger` channel |
| `src/sensor/` | run a duty's fetch script under a read-scoped sandbox |
| `src/bus/` | normalize → coalesce → seed trust → enqueue |
| `src/queue/` | embedded SQLite queue (durable, leased, coalescing, debounce) |
| `src/harness/` | the consciousness loop, dispatch, the wall integration, modules, workers, Reflect |
| `src/state/` + `src/state_prompt.rs` | consciousness states, tool classes, state-prompt parsing |
| `src/runtime/` | the `RuntimeClient` seam + the OpenClaude bridge driver + the wall responder |
| `src/sandbox/` | the `Sandbox` seam (host / docker) |
| `src/config/` | `dack.config.yaml` parsing + the trust lattice |
| `src/secrets/` | secrets-provider broker |
| `src/repo/` | the soul git host (plain-git / signed `gitlawb://`) |
| `src/identity/` | identity resolution + signing (native Ed25519 / legacy `gl`) |
| `src/runlog/` | the agent "syslog" |
| `src/stimuli/` | duty (`STIMULUS.md`) registry + hot-reload |
| `src/webserver/` | the localhost webhook listener |
| `src/model/` | the core domain types (Stimulus, Baton, proposal, runlog) |

The design rationale behind these choices is documented inline — in the module-level doc comments
throughout `src/` and in the rest of this `docs/` tree.
