#!/bin/sh
# One-command demo boot: gateway env from secrets/, soul prepared, daemon up (modules ride along:
# telegram ingress, notify router, booking ingest). Booking Desk runs separately:
#   .venv/bin/python3 demo/booking_desk.py
set -e
cd "$(dirname "$0")/.."

# Gateway env (any OpenAI-compatible endpoint). Explicit env wins; else secrets/openrouter.json
# {apiKey, apiUrl, model} — apiUrl may be the /messages form, we normalize to the base.
if [ -f secrets/openrouter.json ]; then
  export OPENAI_BASE_URL="${OPENAI_BASE_URL:-$(.venv/bin/python3 -c "import json;u=json.load(open('secrets/openrouter.json'))['apiUrl'];print(u[:-9] if u.endswith('/messages') else u)")}"
  export OPENAI_API_KEY="${OPENAI_API_KEY:-$(.venv/bin/python3 -c "import json;print(json.load(open('secrets/openrouter.json'))['apiKey'])")}"
  export OPENAI_MODEL="${OPENAI_MODEL:-$(.venv/bin/python3 -c "import json;print(json.load(open('secrets/openrouter.json'))['model'])")}"
fi
[ -n "$OPENAI_API_KEY" ] || { echo "no gateway: set OPENAI_BASE_URL/OPENAI_API_KEY/OPENAI_MODEL or provide secrets/openrouter.json"; exit 1; }
echo "gateway: $OPENAI_BASE_URL model: $OPENAI_MODEL"

# Soul runtime prerequisites (idempotent): a git repo with runlogs/workspaces ignored, an identity.
mkdir -p soul/runlogs soul/workspaces mail/inbox .sibyl-data
if [ ! -d soul/.git ]; then
  git -C soul init -q
  git -C soul config user.name "Nora" && git -C soul config user.email "nora@localhost"
  printf 'runlogs/\nworkspaces/\nidentity/\n' > soul/.gitignore
  git -C soul add -A && git -C soul commit -qm "seed soul"
fi
BIN=dack-engine/target/release/dack
[ -x "$BIN" ] || BIN=dack-engine/target/debug/dack
[ -x "$BIN" ] || { echo "build first: cargo build --release --manifest-path dack-engine/Cargo.toml"; exit 1; }
[ -f identities/operator/identity.pem ] || "$BIN" keygen --dir identities/operator --role operator
[ -f soul/identity/identity.pem ] || "$BIN" keygen --dir soul/identity --role soul

exec "$BIN" --config config/dack.config.yaml run
