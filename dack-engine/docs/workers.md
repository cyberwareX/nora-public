# Delegated workers (subagents)

The agent can hand a real build/research job to a **worker** — a fresh, keyless agent invocation the
harness launches in its own workspace. A worker is sandboxed compute the agent *wields*, never a mind it
*extends*: no DID, no posting/settling capabilities, no soul access. The agent stays the only principal.

## How it works

1. From an act state, the agent emits a `spawn { agent, brief }` in its structured output instead of
   doing the work itself. `agent` names a definition under the soul's `agents/` directory.
2. The harness launches it **detached** as a separate runtime invocation: a temporary workspace, a
   tool scope that allows file writes and shell but **no** post/settle capabilities and no soul access,
   and the agent def's prompt plus the brief.
3. When the worker finishes, its summary re-enters the agent as a new, untrusted `worker_completion`
   stimulus (the return firebreak) — to be judged like any other input, never as a trusted
   continuation.

A worker's only writable area is its workspace; the soul's integrity tripwire sees zero soul change, so
a worker can't tamper with the agent's identity.

## Agent definitions

A worker is defined by a markdown file in the soul's `agents/` directory — frontmatter plus a system
prompt (the body):

```markdown
---
description: A sandboxed coding worker. Builds code in an isolated workspace; no soul/voice/wallet.
tools: [Read, Write, Edit, Glob, Grep, Bash]
model: your-model-id
maxTurns: 60
isolation: docker
volumes: [{ source: memory }]
---
You are a coding worker. Build to the brief inside your workspace; create files with paths
relative to your working directory; never reach outside it.
```

| Field | Meaning |
|---|---|
| `description` | Shown to the agent when it chooses whom to delegate to. |
| `tools` | The worker's tool allowlist. |
| `model` | The worker's model (`inherit` to use the runtime default). |
| `maxTurns` | Turn cap for the worker's loop. |
| `isolation` | `docker` runs the worker in a container (needs `runtime.worker_sandbox.enabled`); `host` (or absent) runs it on the host. |
| `volumes` | Extra **read-only** mounts for a containerized worker: soul-relative `source`, optional container `target` (default `/mnt/<basename>`). E.g. mount `memory` so a researcher can read the agent's notes but never write them. |

## OS isolation (Docker)

By default a worker runs on the host: it is bounded by the wall and a throwaway workspace, but its shell
is not OS-confined. Enable `runtime.worker_sandbox` and set an agent's `isolation: docker` to run its
bridge inside a container instead:

- the per-run workspace is bind-mounted read-write,
- the agent def's `volumes` are mounted read-only,
- the rest of the host is unreachable (read-only rootfs, dropped capabilities, no host mount),
- the container is reaped on completion or timeout.

Build the image once and point the config at it:

```sh
docker build -f Dockerfile.worker -t dack/worker:latest .
```

```yaml
runtime:
  worker_sandbox:
    enabled: true
    image: dack/worker:latest
```

The container's filesystem and process isolation are real: a worker's Bash cannot read or write the
host, only its workspace and the read-only volumes you grant it.

### Platform note

The OpenClaude SDK runs some shell commands in its own nested sandbox, which on Linux uses
**bubblewrap** (installed in `Dockerfile.worker`) and needs **unprivileged user namespaces** enabled on
the host kernel — the norm on a Linux server. Docker Desktop on macOS disables unprivileged user
namespaces, so containerized workers are best verified on a Linux host. With `worker_sandbox.enabled:
false`, workers run on the host everywhere.

## Configuration

See [configuration](configuration.md#runtimeworker_sandbox--docker-isolation-for-delegated-workers) for
the `worker_sandbox` fields. The workspace lives under the soul's `workspaces/` directory (which must be
gitignored — the harness refuses to start with the Docker sandbox enabled otherwise, since the integrity
tripwire would otherwise revert worker output); old workspaces are swept at boot.
