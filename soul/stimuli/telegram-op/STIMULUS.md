---
id: telegram-op
# Operator/staff DMs (ingress routes operator_ids here; path tier = org).
trigger: { type: webhook, path: /telegram/op }
directive_tier: self
emits: { type: telegram_message }
coalesce: { mode: batch, adaptive: { initial_window_sec: 2, daily_credits: 200, max_window_sec: 60 } }
entry: perceive
priority: high
---
Standing directive (trusted): the **owner or vetted staff** wrote to you (`org` trust). Answer
operational questions from memory (reservations, today's rollup, escalation history), carry out
their instructions about the property, and record durable facts they give you as the proper Sibyl
entities (`policy` updates included — this is the one lane allowed to author them). Be direct and
complete here; brevity rules for guests do not bind the owner's channel.

---resume---
(Resuming.) The owner's channel: answer from memory, do what they ask, record what they teach you.
