#!/usr/bin/env python3
"""Booking-email ingest — the deterministic lane that keeps Sibyl authoritative.

Watches a local maildir (`--transport dir`, the demo default: anything that can write a file can
be a booking source) for RFC-822 booking emails, parses them STRICTLY, writes the Sibyl entities
+ `today:` rollups (see sibyl_store), then wakes the agent with a summary POST to the harness's
`/bookings` webhook. IMAP (`--transport imap`) covers real mailboxes: same parser, same writes.

Design rules carried from a sibling deployment's incident history:
- Parse credulously NEVER: a booking needs code+unit+both dates or it is not a booking — it's an
  `alert` to the operator, not a guess. (Their loose matcher was wrong on 30 of 31 mails and
  swallowed a gas-leak warning.)
- The source mail is never destroyed: processed files MOVE to mail/processed|rejected/, so a
  human can always re-read what the machine decided.
- Dangerous-looking mail (account warnings, suspensions) is escalated, never auto-handled.
"""
from __future__ import annotations

import argparse
import email
import email.policy
import json
import os
import re
import sys
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from notifylane import notify  # noqa: E402
from sibyl_store import client, record_booking, record_cancellation  # noqa: E402

HARNESS = os.environ.get("HARNESS_WEBHOOK", "http://127.0.0.1:8792")
BOOKINGS_PATH = os.environ.get("BOOKINGS_PATH", "/bookings")
BOT_USERNAME = os.environ.get("NORA_BOT_USERNAME", "nora_demo_bot")

MAIL_DIR = Path(os.environ.get("MAIL_DIR", "mail"))
INBOX = MAIL_DIR / "inbox"
PROCESSED = MAIL_DIR / "processed"
REJECTED = MAIL_DIR / "rejected"

CODE_RE = re.compile(r"^[A-Z0-9][A-Z0-9-]{2,30}$")
DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
UNIT_RE = re.compile(r"^[A-Z0-9-]{1,12}$")
# Account-threatening subjects → operator alert, untouched otherwise (never auto-'handle' these).
URGENT_RE = re.compile(r"warning|suspend|deactivat|violat|verify your account|missing payment", re.I)


def parse_booking(raw: bytes) -> dict | None:
    """A booking email → dict, or None when this is NOT a parseable booking. Strict on the key
    (header or Code: line) and honest about partial fields (`fields_missing`)."""
    msg = email.message_from_bytes(raw, policy=email.policy.default)
    body = msg.get_body(preferencelist=("plain",))
    text = body.get_content() if body else ""
    subject = msg.get("Subject", "")

    code = (msg.get("X-Booking-Code") or "").strip()
    if not code:
        m = re.search(r"^Code:\s*(\S+)", text, re.M)
        code = m.group(1).strip() if m else ""
    if not CODE_RE.match(code):
        return None

    def field(label: str, pattern: re.Pattern) -> str | None:
        m = re.search(rf"^{label}:\s*(.+)$", text, re.M)
        v = m.group(1).strip() if m else None
        return v if v and pattern.match(v) else None

    kind = "cancelled" if re.search(r"cancel", subject, re.I) else "new_booking"
    if kind == "cancelled":
        return {"event": "cancelled", "code": code}

    unit = field("Unit", UNIT_RE)
    check_in = field("Check-in", DATE_RE)
    check_out = field("Check-out", DATE_RE)
    guest_name = None
    m = re.search(r"^Guest:\s*(.+)$", text, re.M)
    if m:
        guest_name = m.group(1).strip()[:80]

    required = {"unit": unit, "check_in": check_in, "check_out": check_out}
    missing = [k for k, v in required.items() if not v]
    if missing:
        # A keyed mail with missing essentials is a MALFORMED booking — surface, don't guess.
        return {"event": "malformed", "code": code, "fields_missing": missing, "subject": subject}
    return {
        "event": "new_booking",
        "code": code,
        "unit": unit,
        "check_in": check_in,
        "check_out": check_out,
        "guest_name": guest_name,
        "fields_missing": [],
        "source": "email",
    }


def wake_harness(payload: dict) -> bool:
    body = json.dumps({**payload, "dedup_key": payload.get("code", "bookings")}).encode()
    try:
        req = urllib.request.Request(
            HARNESS + BOOKINGS_PATH, data=body, headers={"Content-Type": "application/json"}
        )
        with urllib.request.urlopen(req, timeout=10) as r:
            return 200 <= r.status < 300
    except Exception as e:  # noqa: BLE001
        print(f"[ingest] harness wake failed: {e}", flush=True)
        return False


def handle_mail(raw: bytes, origin: str) -> str:
    """One email through the pipeline. Returns disposition: booked|cancelled|alerted|rejected."""
    subject = email.message_from_bytes(raw, policy=email.policy.default).get("Subject", "")
    parsed = parse_booking(raw)

    if parsed is None:
        if URGENT_RE.search(subject or ""):
            notify("alert", "Suspicious platform mail", f"{origin}: {subject!r} — review by hand")
            return "alerted"
        print(f"[ingest] not a booking (ignored): {origin} {subject!r}", flush=True)
        return "rejected"

    if parsed["event"] == "malformed":
        notify(
            "alert",
            f"Malformed booking mail {parsed['code']}",
            f"missing {parsed['fields_missing']} — {origin} {parsed['subject']!r}; NOT recorded",
        )
        return "rejected"

    c = client()
    if parsed["event"] == "cancelled":
        prev = record_cancellation(c, parsed["code"])
        summary = {"event": "cancelled", "code": parsed["code"], "known": prev is not None}
        notify("info", f"Booking cancelled: {parsed['code']}", json.dumps(summary))
        wake_harness(summary)
        return "cancelled"

    record_booking(c, parsed)
    deep_link = f"https://t.me/{BOT_USERNAME}?start={parsed['code']}"
    summary = {
        "event": "new_booking",
        "code": parsed["code"],
        "unit": parsed["unit"],
        "guest_name": parsed["guest_name"],
        "check_in": parsed["check_in"],
        "check_out": parsed["check_out"],
        "deep_link": deep_link,
    }
    notify(
        "info",
        f"New booking {parsed['code']}",
        f"{parsed['guest_name'] or 'guest'} → {parsed['unit']} {parsed['check_in']}→{parsed['check_out']}\n"
        f"guest link: {deep_link}",
    )
    wake_harness(summary)
    return "booked"


def drain_dir() -> int:
    """Process every .eml in the inbox once. The file MOVE is the seen-set (inspectable, atomic
    on one filesystem, and — the sibling lesson — done AFTER handling, so a crash re-processes
    rather than silently drops; Sibyl upserts make the retry harmless."""
    for d in (INBOX, PROCESSED, REJECTED):
        d.mkdir(parents=True, exist_ok=True)
    n = 0
    for p in sorted(INBOX.glob("*.eml")):
        disposition = handle_mail(p.read_bytes(), p.name)
        dest = PROCESSED if disposition in ("booked", "cancelled") else REJECTED
        p.rename(dest / p.name)
        print(f"[ingest] {p.name} → {disposition}", flush=True)
        n += 1
    return n


def drain_imap() -> int:
    """Real-mailbox lane: unseen messages from IMAP, same pipeline. Config via env:
    IMAP_HOST/IMAP_USER/IMAP_PASS_FILE (app password), optional IMAP_FOLDER (INBOX)."""
    import imaplib

    host, user = os.environ.get("IMAP_HOST"), os.environ.get("IMAP_USER")
    pass_file = os.environ.get("IMAP_PASS_FILE")
    if not (host and user and pass_file):
        print("[ingest] imap transport needs IMAP_HOST/IMAP_USER/IMAP_PASS_FILE", flush=True)
        return 0
    password = Path(pass_file).read_text().strip()
    M = imaplib.IMAP4_SSL(host)
    M.login(user, password)
    M.select(os.environ.get("IMAP_FOLDER", "INBOX"))
    _, data = M.search(None, "UNSEEN")
    n = 0
    for num in data[0].split():
        _, msg_data = M.fetch(num, "(RFC822)")
        handle_mail(msg_data[0][1], f"imap:{num.decode()}")
        # \Seen is set by the fetch; the mailbox itself is never expunged by us.
        n += 1
    M.logout()
    return n


def main() -> None:
    ap = argparse.ArgumentParser(description="booking-email → Sibyl → harness wake")
    ap.add_argument("--transport", choices=["dir", "imap"], default="dir")
    ap.add_argument("--watch", action="store_true", help="poll forever (module mode)")
    ap.add_argument("--interval", type=float, default=5.0)
    args = ap.parse_args()
    drain = drain_dir if args.transport == "dir" else drain_imap
    if not args.watch:
        print(f"[ingest] drained {drain()} mail(s)", flush=True)
        return
    print(f"[ingest] watching ({args.transport}, every {args.interval}s)", flush=True)
    while True:
        try:
            drain()
        except Exception as e:  # noqa: BLE001 — the watcher survives; the operator hears about it
            notify("error", "Booking ingest error", str(e)[:500])
            print(f"[ingest] drain error: {e}", flush=True)
        time.sleep(args.interval)


if __name__ == "__main__":
    main()
