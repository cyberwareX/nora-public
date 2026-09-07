#!/usr/bin/env python3
"""Seed the Sibyl store from demo/property.yaml — units, policies, kb-cases.

Owner-owned truth: re-running is safe (idempotent upserts). The agent never writes these
categories; this script and the owner's org channel are their only authors.
"""
from __future__ import annotations

import sys
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).parent.parent / "ingest"))
from sibyl_store import client  # noqa: E402


def main() -> None:
    spec = yaml.safe_load((Path(__file__).parent / "property.yaml").read_text())
    c = client()
    for name, body in (spec.get("units") or {}).items():
        c.set_entity("unit", str(name).upper(), body)
        print(f"unit {name}: seeded")
    for slug, body in (spec.get("policies") or {}).items():
        c.set_entity("policy", slug, body)
        print(f"policy {slug}: seeded")
    for slug, body in (spec.get("kb_cases") or {}).items():
        c.set_entity("kb-case", slug, body)
        print(f"kb-case {slug}: seeded")
    c.set_entity(
        "reference",
        "business",
        {"business_name": spec.get("business_name"), "host_name": spec.get("host_name")},
    )
    print("reference business: seeded")
    print(f"done → tenant {c.get_tenant()}")


if __name__ == "__main__":
    main()
