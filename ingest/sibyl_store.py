"""The ZERO-LLM side of Nora's memory: deterministic writers for the Sibyl store.

The agent READS these entities (and journals its own events via the MCP); reservations, units,
policies, and the daily rollups are written only here and by the seed — so the structured truth
can never drift on a model's mood. Same DB file + tenant the MCP instances use.
"""
from __future__ import annotations

import os
from datetime import date
from typing import Any

from sibyl_memory_client import DEFAULT_TENANT, MemoryClient

DB = os.environ.get("SIBYL_MEMORY_DB", ".sibyl-data/memory.db")
# One tenant per STORE FILE: the sibyl-memory-mcp server (0.2.1) resolves DEFAULT_TENANT and has no
# SIBYL_TENANT_ID override (the env var in older docs is dead code) — so the writer must use the
# same default or the agent reads an empty tenant. Isolation is the DB path, one store per agent.
TENANT = os.environ.get("SIBYL_TENANT_ID") or DEFAULT_TENANT


def client() -> MemoryClient:
    return MemoryClient.local(DB, tenant_id=TENANT)


def upsert_reservation(c: MemoryClient, code: str, body: dict[str, Any]) -> None:
    """Insert/update a reservation entity. Merges over an existing body so a modification
    email can't silently erase fields it didn't carry (partner lesson: honest partials)."""
    try:
        prev = c.get_entity("reservation", code).get("body") or {}
    except Exception:
        prev = {}
    c.set_entity("reservation", code, {**prev, **body})


def rollup_add(c: MemoryClient, day: str, kind: str, code: str) -> None:
    """Add a booking code to `today:<day>`'s arrivals/departures list (idempotent)."""
    key = f"today:{day}"
    doc = (c.get_state(key) or {}).get("body") or {}
    lst = list(doc.get(kind) or [])
    if code not in lst:
        lst.append(code)
    doc[kind] = lst
    c.set_state(key, doc)


def rollup_remove(c: MemoryClient, day: str, kind: str, code: str) -> None:
    key = f"today:{day}"
    doc = (c.get_state(key) or {}).get("body") or {}
    lst = [x for x in (doc.get(kind) or []) if x != code]
    doc[kind] = lst
    c.set_state(key, doc)


def record_booking(c: MemoryClient, b: dict[str, Any]) -> None:
    """A parsed booking → reservation entity + rollups. `b` needs code/unit/check_in/check_out."""
    upsert_reservation(
        c,
        b["code"],
        {
            "guest_name": b.get("guest_name"),
            "unit": b["unit"],
            "check_in": b["check_in"],
            "check_out": b["check_out"],
            "status": "booked",
            "source": b.get("source", "email"),
            "fields_missing": b.get("fields_missing") or [],
        },
    )
    rollup_add(c, b["check_in"], "arrivals", b["code"])
    rollup_add(c, b["check_out"], "departures", b["code"])


def record_cancellation(c: MemoryClient, code: str) -> dict[str, Any] | None:
    """Cancel: status flip + drop from rollups. Returns the prior body (None if unknown)."""
    try:
        prev = c.get_entity("reservation", code).get("body") or {}
    except Exception:
        return None
    c.set_entity("reservation", code, {**prev, "status": "cancelled"})
    if prev.get("check_in"):
        rollup_remove(c, prev["check_in"], "arrivals", code)
    if prev.get("check_out"):
        rollup_remove(c, prev["check_out"], "departures", code)
    return prev


def today_str() -> str:
    return date.today().isoformat()
