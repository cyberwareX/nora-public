# Channels (talking to the world)

The harness reaches the outside world through two seams that stay cleanly separated from the core:

- **Inbound** events arrive as POSTs to the localhost webhook listener, each on a path whose configured
  trust tier seeds the resulting cycle.
- **Long-running channel adapters** (a Telegram long-poll loop, a websocket client) run as **modules** —
  side processes the harness owns: it spawns them, injects their secrets, and keeps them alive.

The core harness never parses a channel protocol. An adapter normalizes its protocol into a generic
webhook POST; the harness only sees "a localhost webhook fired at tier X." This keeps channel
specifics out of the trust-critical code.

## Webhook trust anchors

`config.webhooks` maps a webhook path to the trust tier a stimulus on that path is seeded at:

```yaml
webhooks:
  "/telegram/op":      org       # the operator's own messages
  "/telegram/trusted": org       # a vetted group
  "/telegram/pub":     public    # strangers, public groups — reply-only, can't trade
```

The path is the provenance contract the wall enforces. The agent cannot write `config.webhooks`, and
the listener is localhost-only — so only a local adapter can post, and the tier is the operator's call,
not the sender's. A duty (`stimuli/*/STIMULUS.md`) with `trigger: { type: webhook, path: … }`
registers the route; the POSTed body becomes the stimulus payload.

## Modules (the supervisor)

A **module** is operator-trusted plumbing declared in `config.modules`. At boot the harness spawns each
enabled module, injects its declared secrets (resolved fresh on every restart, so a rotated token is
picked up), and supervises it: an exited child is restarted with exponential backoff (1s→30s; a child
that stays up resets the curve), and on shutdown every child is killed.

```yaml
modules:
  - name: telegram-ingress
    command: [bun, run, openclaude-bridge/telegram-ingress.ts]
    secrets: [telegram_bot]          # injects TELEGRAM_BOT_TOKEN
    env: { TELEGRAM_INGRESS_CONFIG: telegram-ingress.config.json }
```

This is the single-config contract for hosting: one `dack run` starts the agent's whole runtime — its
mind *and* its channels. A module touches no trust lattice; it only carries normalized events to the
webhook, where the trust contract applies. Adding a channel is a `modules:` entry, a webhook
path→tier, and the adapter script — never a harness change. See
[configuration](configuration.md#channels-modules) for the fields.

## The bundled Telegram adapter

`openclaude-bridge/` ships a reference Telegram integration in three pieces, illustrating the pattern.

### Ingress (a module)

`telegram-ingress.ts` long-polls the bot, decides each message's trust by **who sent it** using its own
config (not the harness), and POSTs a normalized message to the matching webhook path. Routing
precedence:

1. the operator's user id (anywhere) → the operator path,
2. a known group's chat id (a `groups` map in the adapter config) → that group's path,
3. everyone else → the public path.

The adapter's config (`telegram-ingress.config.json`, operator-owned and gitignored) is where the
Telegram-specific rails live — chat/user → webhook path. The harness only knows path → tier.

Its format (every key optional; defaults shown — the file it reads comes from the module's
`TELEGRAM_INGRESS_CONFIG` env, else `telegram-ingress.config.json`):

```json
{
  "harness_webhook": "http://127.0.0.1:8787",
  "operator_user_id": 111111111,
  "op_path": "/telegram/op",
  "pub_path": "/telegram/pub",
  "groups": { "-1001234567890": "/telegram/trusted" }
}
```

- `harness_webhook` — base URL of the harness listener (match `webhook_addr`).
- `operator_user_id` — your Telegram numeric user id; a message from it (in any chat) routes to
  `op_path`. `null`/omitted = no operator recognition.
- `groups` — a `chat_id → webhook path` map; a message in a listed group routes to that path.
- `op_path` / `pub_path` — operator and fallback (everyone-else) paths.

Routing per message: operator id → `op_path`; else a `groups[chat_id]` hit → that path; else `pub_path`.
Each path's **trust tier** is set by `config.webhooks` in `dack.config.yaml` (above), never here. The
adapter needs the bot token in its env (`TELEGRAM_BOT_TOKEN`, injected via the module's
`secrets: [telegram_bot]`; it also falls back to `secrets/telegram.token`). Each POST it sends carries
`{chat_id, message_id, text, from_username, from_user_id, chat_type, sent_at, dedup_key}` — where
`dedup_key` (the chat id) is the per-conversation coalescing key.

### Egress — reply (destination-locked)

`telegram-mcp.ts` is an MCP server exposing one tool, `reply{text}`. The harness injects the source
chat into its env via `scope_env`, so the model passes only the text and physically cannot reach any
other chat. This is open to any tier — a public stranger can be answered in their own thread, and
nowhere else.

### Egress — proactive send (authorized)

`telegram-send-mcp.ts` exposes `send_message{to, text}` for proactively messaging a **named**
destination the operator pre-registered. It is gated `min_trust: org`, so a public/stranger cycle never
gets it — no one can prompt-inject the agent into spamming the operator's group. `to` resolves against
an operator map, so even an authorized cycle can only reach known chats, never a raw id.

Together these mirror the general pattern for any channel: a destination-locked reply that's open to
everyone, and an authorized proactive send that's gated by trust. See
[capabilities](capabilities.md#locking-a-capability-to-per-cycle-data-scope_env) for the `scope_env`
and `min_trust` mechanisms.

## The bundled Twitter/X integration

X is reached as MCP capabilities rather than a long-running module — inbound is a polling sensor, egress
is a pair of MCP servers, and the same trust split applies.

- **Inbound** — a duty's fetch script (`stimuli/*/STIMULUS.md`) polls mentions / a thread and emits a
  stimulus. Incoming tweets seed `public`, so reading them taints a cycle and it can only *reply*, never
  trade or self-edit on the strength of a stranger's tweet. In Perceive the agent can also pull X context
  on demand via the read server.
- **Egress — write (`twitter-mcp.ts`)** — exposes `post`, `reply`, and the deliberate amplification tools
  `retweet` / `unretweet` / `quote_tweet`. These are **Post**-class (Express only) and the amplification
  tools are listed in `dry_run.block`, so they are previewed (logged, not sent) until the operator clears a
  specific action — amplification is meant to be rare and considered.
- **Egress — read (`twitter-read-mcp.ts`)** — `get_followers` / `get_following` / `get_liking_users` and
  timeline reads. A `public` capability: useful for context but it taints, so a digest that calls it cannot
  then write long-term memory in the same cycle.

**There is deliberately no follow tool.** Following is a stronger social signal (and an account-risk
surface) that stays a human decision: the `twitter-digest` job, when it spots an account worth following,
*suggests* it to the operator over Telegram (`telegram-send`), and the operator follows manually. See
[concepts](concepts.md#memory-short-term-vs-long-term) for the digest pattern and
[capabilities](capabilities.md) for the trust classes.

## Coalescing a busy channel

A high-volume channel (a group with read-everything enabled) can flood the single-flight loop. A duty
can fold a burst into one wake with a `coalesce` policy in its `STIMULUS.md` frontmatter, keyed per
chat and debounced — so the agent wakes once on "the conversation since I last looked," not once per
message. See [authoring souls](authoring-souls.md#coalescing).
