"""Tiny producer client for the op-notify lane. Fire-and-forget: notification failure must never
break the pipeline that produces it (it's the lane that REPORTS breakage, not a dependency)."""
from __future__ import annotations

import json
import os
import urllib.request

NOTIFY_URL = os.environ.get("NOTIFY_URL", "http://127.0.0.1:8794/notify")


def notify(severity: str, title: str, body: str, source: str = "ingest", audience: str = "op") -> bool:
    payload = json.dumps(
        {"severity": severity, "audience": audience, "source": source, "title": title, "body": body}
    ).encode()
    try:
        req = urllib.request.Request(
            NOTIFY_URL, data=payload, headers={"Content-Type": "application/json"}
        )
        with urllib.request.urlopen(req, timeout=5) as r:
            return 200 <= r.status < 300
    except Exception as e:  # noqa: BLE001 — deliberately broad: the lane must not throw
        print(f"[notifylane] router unreachable ({e}) — {severity}: {title}", flush=True)
        return False
