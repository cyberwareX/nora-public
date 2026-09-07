# dack-engine documentation

Start with **[getting-started](getting-started.md)**, then **[concepts](concepts.md)** to understand
the safety model. The rest are reference and how-to.

### Learn
- **[Getting started](getting-started.md)** — clone → build → run → talk to it.
- **[Concepts](concepts.md)** — consciousness states, trust/taint, the firebreak. The safety model.
- **[Architecture](architecture.md)** — how the harness is built, end to end.

### Build & run
- **[Configuration](configuration.md)** — every `dack.config.yaml` field, with defaults.
- **[Authoring souls](authoring-souls.md)** — `SOUL.md`, state-prompts, duties, memory, skills.
- **[Operations](operations.md)** — CLI reference, daemon lifecycle, signed soul repos, identities.

### Extend
- **[Capabilities](capabilities.md)** — give the agent tools (MCP servers).
- **[Channels](channels.md)** — connect Telegram or another live channel (modules + webhooks).
- **[Workers](workers.md)** — delegated, sandboxed subagents.
- **[Secrets & sandboxing](secrets-and-sandbox.md)** — keeping creds out of the model; isolating code.

Design rationale and history live inline in the code's module doc comments (`src/`) and across this
`docs/` tree.
