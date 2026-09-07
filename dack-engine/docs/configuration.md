# Configuration reference

`dack.config.yaml` is the operator control plane: the single non-LLM authority surface. The harness
reads it; the agent may read selected fields but can never write it. It is hot-reloadable — most
changes take effect on the next dispatch without a restart.

Keep it **gitignored**. Secret *values* never live here — only references and provider definitions.
A starting point lives in [`dack.config.example.yaml`](../dack.config.example.yaml).

The only required field is `operator_did`. Everything else has a safe default.

```yaml
operator_did: "did:key:z6Mk…"
runtime:
  connector: { type: opengateway, api_url: "https://gateway.example/v1", api_key: "…" }
  model: "your-model-id"
```

---

## Identity & trust

| Field | Type | Default | Meaning |
|---|---|---|---|
| `operator_did` | string | **required** | The trusted operator DID. A `dack say` instruction is signed with the operator key and verified against this DID — that is what makes a stimulus `operator_signed`. |
| `trust_tiers` | list | `public→express`, `self→reflect`, `operator_signed→reflect` | The trust lattice (see [concepts](concepts.md)). Ordered low→high; rank = position. Each entry is `{ name, reaches }` where `reaches` is the highest consciousness state a cycle at that tier may walk to. The three names `public` / `self` / `operator_signed` must exist in any custom lattice. An unknown tier name fails safe (lowest rank). |
| `signed_scripts` | map | `{}` | Sensor-script provenance: `sha256(script-source-hex) → trust tier`. A sensor whose source hashes to a listed entry seeds its cycle at that tier; an unsigned or since-edited script seeds `public`. This is operator code-review-as-signing. |
| `webhooks` | map | `{}` | Per-webhook-path trust anchor: `"/path" → trust tier`. A stimulus arriving on a registered webhook path is seeded at that path's tier. An unlisted path seeds `public`. The localhost-only listener means only a local adapter can post. |

```yaml
trust_tiers:
  - { name: public, reaches: express }
  - { name: org,    reaches: settle }   # a custom tier between public and self
  - { name: self,   reaches: reflect }
  - { name: operator_signed, reaches: reflect }

webhooks:
  "/telegram/op":  org
  "/telegram/pub": public
```

---

## Capabilities (MCP servers)

`mcp_servers` is the registry of tools the agent can use. The agent can never add one — that would be
self-granting authority. See [capabilities](capabilities.md) for the full model.

Each entry:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | required | Server name. Its tools are addressed as `mcp__<name>__<tool>`. |
| `transport` | object | required | `{ type: stdio, command, args }` (a local process) or `{ type: http, url }` (a remote server). |
| `auth` | object | none | `{ secret, env?, key?, header?, scheme? }`. `secret` names a `secrets_providers` entry; its token is injected into the server's stdio `env` (named by `env`) or HTTP header (`header`/`scheme`, default `Authorization: Bearer`). The token reaches the server, never the agent context. |
| `tier` | enum | required | `read` (safe in every state), `post` (Express — reversible), or `settle` (Settle only — irreversible). The wall classifies the server's tools from this. |
| `tools` | list | `[]` = all | Per-server tool allowlist (bare names). When set, the wall denies any other tool under `mcp__<name>__` fail-closed. Use it to expose a read-only subset of an endpoint that otherwise serves write tools to every token. |
| `trust` | tier | `self` | The taint label of the *data* this server puts in play. Calling any of its tools degrades the cycle's trust to (at most) this tier. Anything touching the public world must be `trust: public`. |
| `min_trust` | tier | none | Authorization floor: the server is only assembled for a cycle whose current (post-taint) trust ranks `>=` this tier. A high-trust cycle that degrades mid-walk loses the capability. |
| `scope_env` | map | `{}` | `{ ENV_VAR: payload_field }`. At assembly the harness reads `payload_field` from the waking stimulus and injects it into this server's env — locking the capability to per-cycle data the model can't supply (e.g. a reply destination). |
| `env` | map | `{}` | Static env for the server (non-secret operator config the server needs, e.g. a named-destination map). |

```yaml
mcp_servers:
  - name: twitter
    transport: { type: stdio, command: bun, args: [run, openclaude-bridge/twitter-mcp.ts] }
    auth: { secret: x, env: X_BEARER_TOKEN }
    tier: post
    trust: public
  - name: cove-read
    transport: { type: http, url: "https://cove.example/api/mcp" }
    auth: { secret: cove_read }
    tier: read
    trust: self
    tools: [get_balance, get_positions, simulate_swap]
```

### `tier_policy` — the operator half of the capability handshake

A state-prompt requests servers in its `mcp:` frontmatter; `tier_policy` admits them. Keyed by
consciousness state (`perceive` / `express` / `settle` / `reflect`). An unconfigured state is locked
(nothing grantable).

| Field | Type | Default | Meaning |
|---|---|---|---|
| `import` | list | `[]` | The `mcp_servers` names a state-prompt at this tier may import. |
| `deny` | list | `[]` | Names explicitly refused even if a prompt requests them. |
| `mcp_whitelist` | bool | `true` | `false` = an **open** tier: a state-prompt may also inline any public, secret-less read MCP (forced read-tier; it can never self-grant a post/settle tool). |
| `model` | string | global `runtime.model` | The operator's default model for this tier (the model split). |
| `allow_model_override` | bool | `false` | Whether a state-prompt at this tier may name its own `model:`. |

```yaml
tier_policy:
  perceive: { mcp_whitelist: false, import: [cove-read, twitter-read] }
  express:  { import: [twitter, telegram] }
  settle:   { import: [cove-trading, cove-read], model: "strong-model-id" }
  reflect:  { import: [], allow_model_override: true }
```

### `post_tools` / `settle_tools`

Tool-name prefixes the wall classifies independently of the registry — `post_tools` → `Post`
(Express), `settle_tools` → `SettleTx` (Settle only). They must stay disjoint. Defaults:
`post_tools: [mcp__twitter__]`, `settle_tools: [mcp__bankr__, mcp__dac__]`. Most deployments rely on
the registry `tier` instead and leave these alone.

---

## Secrets

The harness never stores secret values. Two mechanisms:

### `secrets_providers` — short-lived token materialization

A trusted, harness-owned script that fetches/rotates a token on demand and prints a JSON env map. A
duty or capability references it by name; the harness runs it and injects the result.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | required | Referenced from a duty's `secrets:`, a module's `secrets:`, or a capability's `auth.secret`. |
| `command` | list | required | The provider script argv (e.g. `["python3", "secrets-providers/x_oauth2.py"]`). |
| `env` | map | `{}` | Config (paths, refs) passed to the script — not secret values. |
| `keys` | list | `[]` | The env-var names the provider is expected to emit (documentation/validation). |
| `trust` | tier | `self` | The taint label of the data this secret puts in play. A secret that touches the public world (an X bearer) must be `trust: public`. |

### `secrets` — file references

A plain map of `name → reference` (e.g. `soul_did_key: "file:///run/secrets/soul_did_key"`). Resolved
by the harness; the Soul DID key in particular is harness-only and never forwarded to the agent.

### `forwarded_env`

A list of env-var **names** injected into the agent and sensor environment (values come from the
harness process env, never the YAML). Reserved for recoverable values (API keys, handle, limits) — a
leak means rotate, not catastrophe. The Soul DID key is deliberately never listed here.

See [secrets & sandboxing](secrets-and-sandbox.md) for the full secret model.

---

## Channels (`modules`)

`modules` is the supervisor for long-running side processes — channel adapters that carry normalized
events to the harness's localhost webhook. The harness spawns each enabled module at boot, injects its
declared secrets, and restarts it with backoff until shutdown. See [channels](channels.md).

| Field | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | required | Stable id for logs. |
| `command` | list | required | argv (e.g. `[bun, run, openclaude-bridge/telegram-ingress.ts]`). |
| `secrets` | list | `[]` | Secrets-provider names whose env is injected, resolved fresh on each restart. |
| `env` | map | `{}` | Static env (paths, ids, flags). |
| `cwd` | string | the harness process cwd | Working directory for the module. |
| `enabled` | bool | `true` | `false` ⇒ declared but not started. |

```yaml
modules:
  - name: telegram-ingress
    command: [bun, run, openclaude-bridge/telegram-ingress.ts]
    secrets: [telegram_bot]
    env: { TELEGRAM_INGRESS_CONFIG: telegram-ingress.config.json }
```

---

## Runtime

The engine + connector extensibility point. Swapping runtime/provider is one config edit.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `runtime.engine` | object | `{ type: openclaude, bridge_dir: openclaude-bridge }` | Which agent runtime drives the loop. Only `openclaude` is wired (spawns `bun run bridge.ts`). `bridge_dir` is the bridge project directory. |
| `runtime.connector` | object | `{ type: opengateway }` | How the engine reaches models. `opengateway` (OpenAI-compatible): `{ api_url, api_key }` → `OPENAI_BASE_URL`/`OPENAI_API_KEY` (both optional; `None` falls back to the harness env). It also sets `CLAUDE_CODE_USE_OPENAI=1`. `anthropic` (`{ api_key }`, native catalog) parses but is **not yet wired**. |
| `runtime.model` | string | none | The default model id. `tier_policy.<tier>.model` overrides it per state. On `opengateway` the per-state model is routed via the child's `OPENAI_MODEL` env (the SDK rejects a gateway name as `options.model`). |
| `runtime.env` | list | `[OPENAI_API_KEY, OPENAI_BASE_URL, OPENAI_MODEL, …]` | Extra env-var names forwarded into the runtime bridge (values from the harness env). The connector's own creds are applied on top. |

### `runtime.worker_sandbox` — Docker isolation for delegated workers

See [workers](workers.md). When enabled, an async worker whose agent def declares `isolation: docker`
runs its bridge inside a container.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `false` | `false` ⇒ workers run on the host. |
| `image` | string | `""` | The pre-built worker image (built from `Dockerfile.worker`). |
| `memory` | string | `2g` | Container memory cap. |
| `pids_limit` | int | `256` | Container PID cap. |
| `require` | bool | `true` | When Docker/the image is unavailable: `true` ⇒ hard-fail boot (the safety claim holds); `false` ⇒ warn + fall back to host. |

---

## Soul repo & push destinations

The soul is one local git repo (`soul_repo`); `soul_remotes` is the list of places it's pushed after
each cycle. Backend is inferred per URL — `gitlawb://…` signs a ref-update, anything else is a plain
`git push`. Each target is best-effort unless `required: true`, so a flaky mirror never blocks a cycle.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `soul_repo` | string | `.` | Path to the actor bundle (holds `SOUL.md`, `prompts/`, `stimuli/`, `memory/`, …). |
| `soul_remotes` | list | `[]` | Push destinations (see entry fields below). Empty ⇒ local-only (commits, no push). |
| `soul_remote` | string | none | **Legacy** single remote. Still honored — treated as a one-element best-effort `soul_remotes` list. Prefer `soul_remotes`. |
| `gitlawb_node` | string | `https://node.gitlawb.com` | Default node for any `gitlawb://` entry that omits its own `node:` (and for the legacy `soul_remote`). |
| `identities` | object | `{}` | Identity key directories per role: `operator`, `soul`, `builder` (each holds a PKCS8 `identity.pem`). Create with `dack keygen --role <role> --dir <dir>`. The operator key signs `dack say`; the Soul dir attributes soul commits and never enters agent env. |
| `identities.backend` | `local` \| `gitlawb` | `local` | Signing/DID backend. `local` = native Ed25519, no external tools (reads `identity.pem` directly). `gitlawb` = shell out to the `gl` CLI (requires it on `PATH`). Both derive the same `did:key` from the same key. |
| `secrets.soul_did_key` | ref | — | Reference to the Soul DID key (resolved by the harness; never forwarded). |

### `soul_remotes[]` — one push destination

| Field | Type | Default | Meaning |
|---|---|---|---|
| `url` | string | — | The push URL. Scheme selects the backend: `gitlawb://<soul-did>/<repo>` → signed push; `git@…` / `https://…` / local path → plain `git push`. |
| `kind` | `git` \| `gitlawb` | inferred | Force the backend instead of inferring from `url`. |
| `required` | bool | `false` | `true` ⇒ a push failure to this target fails the cycle's push step. `false` ⇒ best-effort (logged, retried next cycle). |
| `node` | string | `gitlawb_node` | gitlawb only — the node this signed push targets. |
| `identity` | string | `soul` | gitlawb only — which `identities.<role>` key signs the push. |
| `auth` | object | none | Plain-git HTTPS only — `{ token_env, username }`. `token_env` names an env var in the daemon's environment holding the access token (e.g. a GitHub PAT), injected as an ephemeral credential at push time; never in config, disk, argv, or agent context. `username` defaults to `x-access-token`. SSH remotes need no `auth`. |

GitHub (or any plain-git host) transport auth is **independent of the soul DID** — the DID is the
commit author; you authenticate the push with an SSH deploy key (no `auth` needed) or an HTTPS PAT via
`auth.token_env`. See [operations → pushing to a GitHub remote](operations.md#pushing-to-a-github-or-any-plain-git-remote).

---

## Behavior & operations

| Field | Type | Default | Meaning |
|---|---|---|---|
| `default_entry` | string | `perceive` | The state-prompt id harness-synthesized stimuli enter at (the `dack say` instruction, the boot back-online ping). |
| `reflect_schedule` | cron | none | Cron for the scheduled Reflect run (e.g. `"0 4 * * *"`). Omit for manual `dack reflect-now` only. |
| `reflect_min_interval_secs` | int | `86400` | Minimum seconds between Reflect (self-modification) runs — enforced for both the scheduled and any transition-reached Reflect. `0` disables. |
| `session_ttl_secs` | int | `3600` | Idle seconds before a sticky engine session is dropped. `0` = never evict. |
| `queue_max_depth` | int \| null | `10000` | Load-shedding cap: max pending queue depth before the **oldest `Low`-priority** items are evicted (Normal+ is the protected floor, never shed; each eviction is logged). `null` = unbounded. Protects against a runaway low-priority fan-out backlog. |
| `baton_ttl_secs` | int \| null | `null` | A deferred baton continuation older than this many seconds is **expired** (dropped + logged) at dispatch instead of acting on a stale context. `null` = never expire by age. |
| `db_path` | string | `dack.sqlite` | The embedded SQLite queue path (ephemeral — losing it loses only the queue). |
| `webhook_addr` | string | `127.0.0.1:8787` | The localhost bind for the webhook listener. Nothing is public without a proxy. |
| `invoke_timeout_secs` | int | `300` | Wall-clock budget for one consciousness invocation incl. the wall round-trips. A hung LLM/bridge elapses here. A **zero-completion timeout** (the model did nothing) is then re-scheduled up to 3× with backoff — *unless* the wake already took an outward action (post/trade) or was a Reflect, which fail terminally to avoid a double-effect. |
| `reflect_invoke_timeout_secs` | int \| null | `null` | Optional **longer** budget (seconds) for a **Reflect** invocation (the daily self-modification cycle — may chew through prompts/skills/memory), e.g. `900` = 15 min. `null` ⇒ Reflect uses `invoke_timeout_secs`. Applies only to Reflect. |
| `session_max_context_tokens` | int \| null | `null` | Evict a sticky engine session once its model context exceeds this many tokens (the next wake starts fresh) — bounds context bloat. `null` = off (only the idle TTL applies). |
| `memory_write_min_trust` | string | `org` | Trust floor for writing **long-term `memory/`**. A cycle below this tier is denied the write (it can still leave a short-term tag-note). Keeps untrusted in-run cycles from laundering into long-term memory; the digest jobs run at `self` and clear it. |
| `tag_notes_max` | int | `200` | Cap on the short-term **tag-notes** catalogue (`runlogs/tag-notes.ndjson`): only the last N notes are kept. |
| `runlog_retention_days` | int | `10` | **Short-term memory retention**: keep only the last N `runlogs/<date>.md` day-files; older ones are deleted on boot and day-rollover. Also bounds how far back the recall tools can scan. |

### `log` — structured logging

Operational logging via the `tracing` framework, written to **stdout** (so `docker logs` / an operator
backend can capture it). Distinct from the runlog (which is the agent's own memory). `RUST_LOG` overrides
`level` for per-module control (e.g. `RUST_LOG=dack::harness=debug`).

```yaml
log:
  level: info     # trace | debug | info | warn | error
  format: auto    # auto (pretty TEXT on a TTY, JSON when piped/containerized) | text | json
```

Each cycle's log lines carry a correlation span (`cycle{stim=…}`), and every model call emits a latency
line (`elapsed_ms`, token counts) — so a run is traceable end-to-end, and ready for the upcoming
concurrent scheduler.

### `dry_run`

A testing switch: when `enabled`, the wall denies any tool whose name starts with a `block` prefix —
the agent composes the action (visible in the runlog) but it never executes. Tool-level and uniform
across every MCP, so a no-real-trade run can still allow reads and `simulate_swap`.

```yaml
dry_run:
  enabled: true
  block: [mcp__twitter__post, mcp__twitter__reply, mcp__cove-trading__buy_token]
```
