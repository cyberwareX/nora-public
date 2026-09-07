---
id: bookings
# The zero-LLM mail ingest POSTs parsed booking events here AFTER writing them to Sibyl
# (path tier = org: our own parser, vetted data). The payload is a summary of what changed.
trigger: { type: webhook, path: /bookings }
directive_tier: self
emits: { type: booking_event }
coalesce: { mode: batch, adaptive: { initial_window_sec: 5, daily_credits: 100, max_window_sec: 120 } }
entry: perceive
priority: normal
---
Standing directive (trusted): the booking ingest recorded a change — a new reservation,
a modification, or a cancellation. The Sibyl entities are ALREADY up to date (the ingest wrote
them); your job is the human side. New booking: if the guest's chat is already bound (a `guest`
entity exists), send a short warm welcome with the essentials and the deep link the payload
carries; otherwise brief the owner via `escalate` (severity info) with the confirmation summary
so they can forward the welcome. Cancellations: brief the owner, clear the active-stay doc if
set, and record the event. Never invent details the payload or memory doesn't have.

---resume---
(Resuming.) Booking change, entities already written: welcome the bound guest or brief the owner.
