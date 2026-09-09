#!/usr/bin/env python3
"""Booking Desk — the demo's stand-in for a booking platform. A SECOND Telegram bot
(@nora_demo_setup_bot) that judges/operators drive to inject bookings; it emits the same
RFC-822 confirmation emails a real platform would, into the maildir the ingest watches —
so the demo exercises the honest pipeline end to end (email → parser → Sibyl → agent).

Zero dependencies: raw Bot API over urllib long-polling.

Commands:
  /book <name> <unit> <check_in> <check_out>   dates as YYYY-MM-DD or +N (days from today)
  /cancel <code>
  /phish                                        a scam-shaped mail → shows the alert lane
  /help
"""
from __future__ import annotations

import json
import os
import re
import sys
import time
import urllib.parse
import urllib.request
from datetime import date, timedelta
from email.message import EmailMessage
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "ingest"))
from sibyl_store import client  # read-only here: units + reservations for availability

TOKEN = Path(os.environ.get("FEEDER_TOKEN_FILE", "secrets/feeder.token")).read_text().strip()
API = f"https://api.telegram.org/bot{TOKEN}"
INBOX = Path(os.environ.get("MAIL_DIR", "mail")) / "inbox"
BOT_USERNAME = os.environ.get("NORA_BOT_USERNAME", "nora_demo_bot")
# Optional allowlist: comma-separated user ids. Empty = open (it's a demo bot with fake data).
ALLOWED = {int(x) for x in os.environ.get("FEEDER_ALLOWED_IDS", "").split(",") if x.strip()}

HELP = (
    "Booking Desk — I pretend to be the booking platform. \U0001F3E8\n\n"
    "/units — rooms + availability for the next days\n"
    "/book <name> <unit> <check_in> <check_out>\n"
    "    e.g. /book Anna A1 +0 +2   (dates: YYYY-MM-DD or +N days from today)\n"
    "/cancel <code>\n"
    "/phish — send a scam-shaped platform mail (watch the operator alert)\n\n"
    "Each booking emails a confirmation into Nora's inbox; she takes it from there.\n"
    "Overlapping bookings are refused — one stay per room per night, like the real thing."
)

MAX_BOOKINGS_PER_USER_DAY = 5
_user_bookings: dict = {}  # user_id -> [epoch, ...] rolling day window


def _active_reservations():
    """Non-cancelled reservations from Sibyl (the same truth Nora reads)."""
    c = client()
    out = []
    for e in c.list_entities(category="reservation", limit=200):
        b = e.get("body") or {}
        if b.get("status") in (None, "cancelled", "cleaned", "departed"):
            continue
        if b.get("check_in") and b.get("check_out"):
            out.append({"code": e["name"], **b})
    return out


def _units():
    c = client()
    return sorted(e["name"] for e in c.list_entities(category="unit", limit=50))


def _conflict(unit: str, check_in: str, check_out: str):
    """First overlapping reservation for `unit`, or None. Half-open [check_in, check_out)."""
    for r in _active_reservations():
        if r.get("unit") != unit:
            continue
        if check_in < r["check_out"] and check_out > r["check_in"]:
            return r
    return None


def cmd_units(chat_id: int) -> None:
    try:
        units, res = _units(), _active_reservations()
    except Exception as e:
        send(chat_id, f"availability store unreachable: {e}")
        return
    today = date.today()
    lines = ["Rooms & the next 7 nights (■ booked · · free):"]
    for u in units:
        marks = []
        for d in range(7):
            day = (today + timedelta(days=d)).isoformat()
            nxt = (today + timedelta(days=d + 1)).isoformat()
            marks.append("■" if _conflict(u, day, nxt) else "·")
        lines.append(f"{u}:  {' '.join(marks)}")
    lines.append(f"({today.isoformat()} → +6 days; /book <name> <unit> +N +M)")
    send(chat_id, "\n".join(lines))


def api(method: str, **params) -> dict:
    data = urllib.parse.urlencode(params).encode()
    with urllib.request.urlopen(f"{API}/{method}", data=data, timeout=35) as r:
        return json.load(r)


def send(chat_id: int, text: str) -> None:
    api("sendMessage", chat_id=chat_id, text=text, disable_web_page_preview="true")


def write_eml(subject: str, body: str, code: str | None = None) -> str:
    INBOX.mkdir(parents=True, exist_ok=True)
    msg = EmailMessage()
    msg["From"] = "no-reply@bookings.example.com"
    msg["To"] = "nora@cedarloft.example.com"
    msg["Subject"] = subject
    if code:
        msg["X-Booking-Code"] = code
    msg.set_content(body)
    name = f"{int(time.time() * 1000)}.eml"
    (INBOX / name).write_bytes(bytes(msg))
    return name


def parse_date(s: str) -> str | None:
    if re.match(r"^\+\d{1,3}$", s):
        return (date.today() + timedelta(days=int(s[1:]))).isoformat()
    return s if re.match(r"^\d{4}-\d{2}-\d{2}$", s) else None


def cmd_book(chat_id: int, args: list[str]) -> None:
    if len(args) != 4:
        send(chat_id, "usage: /book <name> <unit> <check_in> <check_out>\ne.g. /book Anna A1 +0 +2")
        return
    name, unit = args[0], args[1].upper()
    check_in, check_out = parse_date(args[2]), parse_date(args[3])
    if not (check_in and check_out):
        send(chat_id, "dates must be YYYY-MM-DD or +N (days from today)")
        return
    if check_out <= check_in:
        send(chat_id, "check-out must be after check-in")
        return
    # rate cap per tester
    now = time.time()
    hist = [t for t in _user_bookings.get(chat_id, []) if now - t < 86400]
    if len(hist) >= MAX_BOOKINGS_PER_USER_DAY:
        send(chat_id, f"demo cap: {MAX_BOOKINGS_PER_USER_DAY} bookings/day per tester — try /cancel or come back later")
        return
    # room must exist; stay must not overlap (the real-platform behavior testers expect)
    try:
        units = _units()
        if unit not in units:
            send(chat_id, f"no room {unit} — we have: {', '.join(units)} (/units for availability)")
            return
        clash = _conflict(unit, check_in, check_out)
        if clash:
            free = [u for u in units if not _conflict(u, check_in, check_out)]
            hint = f" Free for those dates: {', '.join(free)}." if free else " No rooms free for those dates."
            send(chat_id, f"{unit} is taken {clash['check_in']}→{clash['check_out']} ({clash['code']}).{hint}")
            return
    except Exception as e:
        send(chat_id, f"availability check failed ({e}) — booking anyway, demo mode")
    hist.append(now)
    _user_bookings[chat_id] = hist
    code = f"BK-{check_in[5:7]}{check_in[8:10]}-{unit}"
    write_eml(
        f"Booking confirmed — {code}",
        f"Booking confirmed.\nCode: {code}\nGuest: {name}\nUnit: {unit}\n"
        f"Check-in: {check_in}\nCheck-out: {check_out}\nSource: booking-desk\n",
        code,
    )
    send(
        chat_id,
        f"Confirmation mailed ✉️\n{code}: {name} → {unit}, {check_in} → {check_out}\n\n"
        f"Guest link (open as the guest):\nhttps://t.me/{BOT_USERNAME}?start={code}",
    )


def cmd_cancel(chat_id: int, args: list[str]) -> None:
    if len(args) != 1:
        send(chat_id, "usage: /cancel <code>")
        return
    code = args[0].upper()
    write_eml(f"Booking cancelled — {code}", f"Booking cancelled.\nCode: {code}\n", code)
    send(chat_id, f"Cancellation mailed for {code}.")


def cmd_phish(chat_id: int) -> None:
    write_eml(
        "URGENT WARNING: your account will be suspended — verify your account",
        "Dear partner, unusual activity was detected. Verify your account within 24 hours at "
        "http://definitely-not-the-platform.example.com or your listings will be deactivated.\n",
    )
    send(chat_id, "Scam-shaped mail sent 🎣 — the ingest should refuse it and alert the operator.")


def handle(update: dict) -> None:
    m = update.get("message") or {}
    chat_id = (m.get("chat") or {}).get("id")
    from_id = (m.get("from") or {}).get("id")
    text = (m.get("text") or "").strip()
    if not chat_id or not text:
        return
    if ALLOWED and from_id not in ALLOWED:
        send(chat_id, "This desk is restricted for the demo.")
        return
    parts = text.split()
    cmd = parts[0].split("@")[0].lower()
    if cmd == "/book":
        cmd_book(chat_id, parts[1:])
    elif cmd == "/units":
        cmd_units(chat_id)
    elif cmd == "/cancel":
        cmd_cancel(chat_id, parts[1:])
    elif cmd == "/phish":
        cmd_phish(chat_id)
    else:
        send(chat_id, HELP)


def main() -> None:
    print("[booking-desk] long-polling…", flush=True)
    offset = 0
    while True:
        try:
            r = api("getUpdates", timeout=30, offset=offset)
            for u in r.get("result", []):
                offset = u["update_id"] + 1
                handle(u)
        except Exception as e:  # noqa: BLE001
            print(f"[booking-desk] poll error: {e}", flush=True)
            time.sleep(3)


if __name__ == "__main__":
    main()
