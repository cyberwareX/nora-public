# Nora — an open-source AI guest-host, with Sibyl as her memory

A self-hostable host for a short-let property: a Telegram persona on the
[dack-engine](dack-engine/) agent harness, with **[Sibyl-Memory](https://github.com/sibyl-labs)**
as her structured memory. Bookings arrive by **email** — the one integration surface every booking
platform gives you for free — a deterministic parser keeps Sibyl authoritative, and the agent
answers guests from exact-key recall. Operations (arrival notes, cleaning-crew coordination,
checkout confirmations) run as **plain-text SOPs** the owner edits like documents.

Built for the Sibyl Labs hackathon, 2026-09.

## Why it's shaped this way

- **No phone farms, no browser scraping, no reseller APIs.** Telegram + email is the
  lowest-friction stack that actually works, and everything here runs on one box.
- **Sibyl is the memory, not a bolt-on.** A zero-LLM ingest writes `reservation`/`guest` entities
  and `today:` rollups; the agent reads by exact key (schemes taught in
  [`soul/SOUL.md`](soul/SOUL.md)), journals events, and advances a real reservation state machine
  (`booked → in_house → checkout_confirmed → cleaned`).
- **Trust is structural.** Guest chats are `public`-taint: the reply tool is destination-locked to
  the waking chat, escalation destinations are operator config the model can't touch, and guest
  text is data, never instructions.
- **Failures notify a human.** A connector-agnostic notify lane (telegram/email) carries agent
  escalations, ingest alerts, and harness failures to the operator. *(Day-2, in progress.)*

## Layout

| dir | what |
|---|---|
| `dack-engine/` | the harness (v1 + a reliability backport: no model-output shape can drop a cycle) |
| `soul/` | Nora: persona, state prompts, duty SOPs — the text that IS the behavior |
| `channels/` | telegram ingress (+ deep-link capture), destination-locked reply MCP, escalate MCP |
| `ingest/` | booking-email parser → Sibyl writer → harness wake *(Day-2)* |
| `notify/` | the op-notification router *(Day-2)* |
| `demo/` | Booking Desk (feeder bot), seed data, demo script *(Day-2/3)* |
| `config/` | working engine + ingress config |

## Quickstart

```sh
# 1. build + deps
cargo build --release --manifest-path dack-engine/Cargo.toml
(cd channels && bun install) && (cd dack-engine/openclaude-bridge && bun install)
python3 -m venv .venv && .venv/bin/pip install sibyl-memory-mcp pyyaml

# 2. secrets/ (gitignored):
#    telegram.token  — your bot's token (BotFather)
#    feeder.token    — the Booking Desk bot's token (optional, for the demo feeder)
#    model gateway   — export OPENAI_BASE_URL / OPENAI_API_KEY / OPENAI_MODEL
#                      (any OpenAI-compatible endpoint), or drop an openrouter-style
#                      secrets/openrouter.json {apiKey, apiUrl, model}
# 3. your ids (kept out of git): copy config/telegram-ingress.config.json and
#    config/notify.config.json to the repo root and put your telegram user id in
#    operator_ids / op_chat_ids — root copies win over the committed placeholders.

# 4. seed the property, run everything (daemon + ingress + notify router + mail ingest):
.venv/bin/python3 demo/seed_sibyl.py
./demo/run.sh

# 5. (separate terminal) the Booking Desk — the pretend booking platform:
.venv/bin/python3 demo/booking_desk.py
```

Then message your bot on Telegram — or open the Booking Desk and `/book Anna A1 +0 +2`,
follow the guest deep link it prints, and watch Nora greet you by name.

## What's live-proven (not just built)

- Guest Q&A from exact-key Sibyl recall (wifi, parking, policies) — with escalation to the owner
  when memory doesn't know.
- **Operator relay**: answer an escalation in your own chat; Nora delivers to the guest through a
  privileged, allow-listed send tool (only bound guests / reservation chats / escalation sources
  are reachable; unknown ids refuse; identical repeats are suppressed; every attempt audited).
- **Conversation continuity** twice over: the engine's per-chat runlog thread view
  (`context: {tag_key: true}`) plus a Sibyl HOT-tier `chat:<id>:state` doc — returning guests are
  greeted as returning.
- **The notify lane**: harness failures, ingest alerts, and agent escalations reach the operator
  through one connector-agnostic router (telegram + jsonl audit today; email/others are just more
  connectors).
- Booking pipeline: email → strict parser → Sibyl entities + `today:` rollups → agent wake.

## Gotchas we hit so you don't

- **Editing the soul while Nora runs**: the harness's integrity tripwire reverts uncommitted soul
  changes within a cycle — land soul edits as commits in the soul repo (`git -C soul commit`).
- **The Booking Desk is its own process** (`demo/booking_desk.py`) — the daemon doesn't start it.
- **MCP stdio paths are soul-relative**: the bridge chdirs to the soul repo before loading the
  SDK, so server commands/env paths in `mcp_servers` resolve from `soul/` (hence the `../`).
- **Your agent's own memory store must be `trust: self`** — labeling it `public` makes every
  memory read degrade the cycle's trust and (correctly) strips `min_trust`-gated tools like the
  relay. The taint model working as designed, against a mislabel.
- **sibyl-memory-mcp 0.2.1 ignores `SIBYL_TENANT_ID`** (the env var in older docs is absent from
  the code) — the server resolves its default tenant, so any out-of-band writer must use the
  client's `DEFAULT_TENANT`. Isolation is the DB file path, one store per agent.

## Feedback to Sibyl Labs (from building this)

1. `SIBYL_TENANT_ID` is documented but dead in `sibyl-memory-mcp` 0.2.1 — either honor it or
   remove it from the docs (it cost us a debugging round: agent and writer silently split tenants).
2. Per-category **write ACLs** would let an operator make `unit`/`policy`/`reservation`
   ingest-only while the agent keeps `guest`/`kb-case`/journal — today that separation is only
   soul discipline.
3. The `verdict` on zero-result searches is genuinely useful — our soul teaches "count:0 is
   normal" because of it.

## License

MIT — see [LICENSE](LICENSE).
