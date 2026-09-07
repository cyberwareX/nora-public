---
id: daily-ops
# The operations heartbeat: twice daily (09:00 + 17:00 UTC). This directive IS the SOP —
# the owner tunes operations by editing this text, nothing else.
trigger: { type: cron, expr: "0 9,17 * * *" }
directive_tier: self
emits: { type: ops_heartbeat }
entry: perceive
priority: normal
---
Standing directive (trusted): run the day's operations. Truth = `memory_get_state("today:<today>")`
and `memory_get_state("today:<tomorrow>")` (each `{arrivals, departures, cleanings}` of booking
codes), then the `reservation` entities by exact key. Work through this SOP, top to bottom:

1. **Arrivals today** (`status: booked`): if the guest's chat is bound, send a short arrival
   note — check-in time, address pointer, "message me anytime". Unbound guests are the owner's to
   contact: one `escalate` (severity info) listing them.
2. **Departures tomorrow** (`status: in_house`): notify the **cleaning crew** — one `escalate`
   with `audience: cleaners`, listing unit + expected checkout time per departure. Record a
   `cleaning_notice` event per reservation (skip any that already has one for this departure —
   check the journal first; never notice twice).
3. **Departures today** still `in_house` past checkout time: a gentle checkout question to the
   bound guest ("hope the stay was great — have you checked out?"). When a guest HAS confirmed
   (`status: checkout_confirmed` — set by the chat SOP in express), send the cleaning crew the
   go-ahead (`escalate`, `audience: cleaners`), record `cleaning_notice` with `phase: "go"`,
   and advance the reservation to `status: cleaned` once the crew's done-word arrives via the
   owner channel.
4. Anything inconsistent (a departure with no reservation entity, a stuck status, an empty rollup
   on a day that should have activity): one `escalate` (severity alert) describing it — never
   patch data you don't understand.

One pass, act through express, record events, terminate. If both rollups are empty: no action,
terminate quietly.

---resume---
(Resuming.) Same SOP, same order: arrivals, cleaning notices, checkout confirmations, anomalies.
