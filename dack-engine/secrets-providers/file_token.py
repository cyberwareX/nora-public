#!/usr/bin/env python3
"""Generic static-token secrets provider (PRD §7.2): read a token from a gitignored file and
emit it as a JSON env `{KEY: value}` the harness injects. For static API bearers (cove.trade,
etc.) that don't rotate — no OAuth2 machinery. Declared in config; adding a new static-token
secret is a `secrets_providers` entry + a token file, never a harness change.

Config env (NOT secret values — paths/keys):
  TOKEN_KEY  — the env var name to emit (e.g. COVE_READ_TOKEN).
  TOKEN_FILE — path to the file holding the raw token (gitignored).
"""
import json
import os
import sys

key = os.environ.get("TOKEN_KEY")
path = os.environ.get("TOKEN_FILE")
if not key or not path:
    sys.exit("file_token: TOKEN_KEY and TOKEN_FILE are required")
with open(path) as f:
    print(json.dumps({key: f.read().strip()}))
