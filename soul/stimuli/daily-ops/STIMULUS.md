---
id: daily-ops
# The operations heartbeat: twice daily at 09:00 + 17:00 PROPERTY LOCAL (Europe/Vilnius,
# UTC+3 → cron below is UTC 06:00/14:00; adjust both together if the property moves).
trigger: { type: cron, schedule: "0 6,14 * * *" }
directive_tier: self
emits: { type: ops_heartbeat }
entry: ops/heartbeat
priority: normal
---
Standing directive (trusted): the operations heartbeat fired. Your `ops/heartbeat` prompt IS the
SOP — work it top to bottom from the `today:` rollups and the journal, and hand ops/express the
worklist. Empty day ⇒ terminate quietly.

---resume---
(Resuming.) Continue the heartbeat SOP where you left off.
