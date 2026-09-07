Vendored + patched @gitlawb/openclaude 0.18.0 sdk.mjs.
PATCH: emptied the 3 CLI identity-prefix literals ("You are OpenClaude...") so the system
prompt is ONLY our SOUL.md (no coding-agent framing). filter(Boolean) drops the now-empty strings.
NOT patched here: OpenAI-shim penalty passthrough (needs a source build — deferred).
Pin: 0.18.0. Do NOT npm-upgrade the bridge import past this without re-vendoring.
