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

*(Firming up through the hackathon — the short version:)*

```sh
# 1. deps
cargo build --release --manifest-path dack-engine/Cargo.toml
(cd channels && bun install)
pip install sibyl-memory-mcp

# 2. secrets/  → telegram.token (BotFather), model gateway env (any OpenAI-compatible)
# 3. seed demo data, run
python3 demo/seed_sibyl.py
./dack-engine/target/release/dack run --config config/dack.config.yaml
```

Then message your bot on Telegram — or open the Booking Desk and `/book` yourself a stay.
