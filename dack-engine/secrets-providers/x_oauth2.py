#!/usr/bin/env python3
"""X OAuth2 **secrets provider** for DACK — trusted, harness-owned, seamed via
`dack.config.yaml` (`secrets_providers: [{name: x, command: [...x_oauth2.py]}]`).

The harness runs this with the provider's config env and reads a single JSON object
`{"X_BEARER_TOKEN": "<valid access token>"}` from stdout. It owns everything sensitive — the
client secret, the rotating refresh token, and the token store — so a *sensor* never does
(a sensor is arbitrary Reflect-authored code; see docs/secrets-and-sandbox.md).

**One store, everything in one place.** `X_STORE` is a single JSON file holding both the long-
lived app creds AND the rotating tokens:

    {"client_id": …, "client_secret": …,        # app creds — read once, never rewritten
     "refresh_token": …, "access_token": …,      # rotating — the ONLY refresh token there is
     "expires_at": <unix>,                        # access-token expiry (validity by timestamp)
     "cooldown_until": <unix>, "failed_refresh_token": …}   # circuit-breaker state

**Corruption-proof by construction:**
  - **flock** (a sibling `<store>.lock`, never replaced) serializes the whole read→refresh→write
    critical section, so two concurrent materialize() calls (or two daemons) can't torn-write the
    store or double-spend a rotating refresh token.
  - **atomic write** (tmp + os.replace, fsync, mode 0600): a crash never truncates the real store.
  - **merge-preserve**: only the rotating fields are ever rewritten; client_id/client_secret (and
    any unknown keys) are carried through verbatim — token churn can't clobber the app creds.
  - **no spent-seed fallback**: the refresh token lives in exactly ONE field and is always the
    latest rotated one. There is no stale "bootstrap seed" to wrongly retry.

**Circuit-breaker.** X rotates the refresh token on every refresh and rejects a spent one with
`invalid_grant`. A rejected refresh is NOT retried in a hot loop (that is what tripped the app's
rate limit): we record `cooldown_until` + the `failed_refresh_token` and fail FAST — without
touching the endpoint — until the cooldown elapses. **Re-auth = just paste a fresh `refresh_token`
into the store**: when the store's refresh_token no longer matches `failed_refresh_token` the
breaker auto-clears.

Config env (paths/knobs, never secret values):
  X_STORE        — the single JSON credential+token store (gitignored, mode 0600).
  X_REFRESH_SKEW — seconds-of-life threshold to refresh early (default 300).

Emits nothing to stdout on failure; the reason goes to stderr (which the harness records as this
secret's health) and the process exits non-zero. stdlib only (urllib).
"""
import base64
import fcntl
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

TOKEN_URL = "https://api.twitter.com/2/oauth2/token"

# Cooldowns (seconds): a rejected refresh token needs a human re-auth, so back off HARD; a
# transient (429 / 5xx / network) just needs to stop hammering, so back off softly.
DEAD_COOLDOWN = 3600
TRANSIENT_COOLDOWN = 120

# The only fields this provider ever rewrites; everything else in the store is carried verbatim.
_ROTATING = ("access_token", "refresh_token", "expires_at", "cooldown_until", "failed_refresh_token")


def _store_path():
    p = os.environ.get("X_STORE")
    if not p:
        sys.exit("x_oauth2: X_STORE is required (the single JSON credential store)")
    return p


def _load(path):
    with open(path) as f:
        return json.load(f)


def _atomic_write(path, store):
    """tmp + fsync + os.replace → a reader/crash never sees a torn or truncated store."""
    tmp = path + ".tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(store, f)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)  # atomic on POSIX


def _refresh(client_id, client_secret, refresh_token):
    basic = base64.b64encode(f"{client_id}:{client_secret}".encode()).decode()
    body = urllib.parse.urlencode(
        {"grant_type": "refresh_token", "refresh_token": refresh_token}
    ).encode()
    req = urllib.request.Request(
        TOKEN_URL,
        data=body,
        method="POST",
        headers={
            "Authorization": f"Basic {basic}",
            "Content-Type": "application/x-www-form-urlencoded",
        },
    )
    return json.load(urllib.request.urlopen(req, timeout=25))


def _emit(access_token):
    print(json.dumps({"X_BEARER_TOKEN": access_token}))


def _run(path):
    """Whole critical section — runs under the flock."""
    store = _load(path)
    skew = int(os.environ.get("X_REFRESH_SKEW", "300"))
    now = int(time.time())

    # Fast path (also catches "another holder just refreshed while we waited on the lock"):
    # validity by stored timestamp — burns no API call.
    if store.get("access_token") and store.get("expires_at", 0) - skew > now:
        _emit(store["access_token"])
        return 0

    refresh_token = store.get("refresh_token")
    if not refresh_token:
        sys.stderr.write(f"x: no refresh_token in {path} — re-auth needed (seed the store)\n")
        return 1

    # Circuit-breaker: honor an active cooldown, but ONLY while the refresh token is still the one
    # that failed. A re-seed (different refresh_token) auto-clears the breaker.
    cooldown_until = store.get("cooldown_until", 0)
    failed_rt = store.get("failed_refresh_token")
    if cooldown_until > now and (failed_rt is None or failed_rt == refresh_token):
        left = cooldown_until - now
        reauth = " — RE-AUTH NEEDED (paste a fresh refresh_token)" if left > TRANSIENT_COOLDOWN else ""
        sys.stderr.write(
            f"x: in cooldown {left}s (until {cooldown_until}){reauth}; last error kept in store\n"
        )
        return 1

    client_id, client_secret = store.get("client_id"), store.get("client_secret")
    if not client_id or not client_secret:
        sys.stderr.write(f"x: client_id/client_secret missing in {path}\n")
        return 1

    try:
        tok = _refresh(client_id, client_secret, refresh_token)
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace")[:200]
        dead = e.code == 400 and "invalid_grant" in detail
        cooldown = DEAD_COOLDOWN if dead else TRANSIENT_COOLDOWN
        store.update(cooldown_until=now + cooldown, failed_refresh_token=refresh_token)
        _atomic_write(path, store)
        if dead:
            sys.stderr.write(
                f"x: refresh rejected (invalid_grant) — RE-AUTH NEEDED: paste a fresh "
                f"refresh_token into {path}; cooling down {cooldown}s\n"
            )
        else:
            sys.stderr.write(f"x: refresh HTTP {e.code} ({detail}) — cooling down {cooldown}s\n")
        return 1
    except (urllib.error.URLError, TimeoutError, OSError) as e:
        store.update(cooldown_until=now + TRANSIENT_COOLDOWN, failed_refresh_token=refresh_token)
        _atomic_write(path, store)
        sys.stderr.write(f"x: refresh network error ({e}) — cooling down {TRANSIENT_COOLDOWN}s\n")
        return 1

    # Success — rotate the store (merge-preserve; clear the breaker). Persist BEFORE printing: the
    # old refresh token is now spent, so the new one must be durable first.
    store.update(
        access_token=tok["access_token"],
        refresh_token=tok.get("refresh_token", refresh_token),
        expires_at=now + int(tok.get("expires_in", 7200)),
        cooldown_until=0,
        failed_refresh_token=None,
    )
    _atomic_write(path, store)
    _emit(store["access_token"])
    return 0


def main():
    path = _store_path()
    lock_fd = os.open(path + ".lock", os.O_CREAT | os.O_RDWR, 0o600)
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX)  # serialize read→refresh→write across processes
        return _run(path)
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)


if __name__ == "__main__":
    sys.exit(main())
