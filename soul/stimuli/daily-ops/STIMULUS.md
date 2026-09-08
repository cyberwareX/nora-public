---
id: daily-ops
# The operations heartbeat: twice daily (09:00 + 17:00 UTC). This directive IS the SOP —
# the owner tunes operations by editing this text, nothing else.
trigger: { type: cron, schedule: "0 9,17 * * *" }
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
