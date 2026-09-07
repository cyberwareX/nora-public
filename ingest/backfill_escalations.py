#!/usr/bin/env python3
"""One-shot: journal historical escalations (escalations.jsonl) into Sibyl COLD-tier events, in the
exact shape memory_record_event writes — so the agent can FIND past escalations by chat id instead
of pre-refusing relays. New escalations self-journal per the express SOP; this covers the ones
recorded before that rule existed. Idempotent-enough: skips if an event for (chat, at) exists."""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from sibyl_store import client

c = client()
seen = set()
try:
    for ev in c.read_events(limit=500):
        acted = ev.get("acted") or {}
        b = (acted.get("body") or {}) if isinstance(acted, dict) else {}
        seen.add((str(b.get("chat_id")), b.get("at")))
except Exception:
    pass

n = 0
for line in Path("escalations.jsonl").read_text().splitlines():
    if not line.strip():
        continue
    e = json.loads(line)
    chat = str(e.get("source_chat") or "")
    if not chat or (chat, e.get("at")) in seen:
        continue
    c.write_event(
        acted={"kind": "escalation", "body": {"chat_id": chat, "need": e.get("body", "")[:300], "at": e.get("at")}},
        extra={"category": "guest", "name": chat},
    )
    n += 1
print(f"backfilled {n} escalation event(s)")
