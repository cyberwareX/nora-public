# Getting started

This walks you from a fresh clone to a running agent you can talk to — no gitlawb, no team-internal
tooling required.

## Prerequisites

- **Rust** (stable) + a **C toolchain** (`cc`/clang) — builds the `dack` binary. SQLite is compiled in
  (`rusqlite` bundled), which needs the C compiler; there is no `protoc`/OpenSSL requirement.
- **Bun** (1.x) — runs the runtime bridge (`openclaude-bridge/`), which drives the OpenClaude SDK.
  (Node/npm are **not** used.)
- **git** — the soul and its runlogs are git repos the harness commits to at boot and each cycle.
- **A model endpoint** — an OpenAI-compatible gateway (base URL + key + a model id). Any provider that
  speaks the OpenAI chat API works.
- *Optional:* **python3** — only if you enable the built-in X/Twitter capability or its duties (the
  provider scripts are stdlib-only, no `pip`). **Docker** — only if you turn on sandboxed workers
  (`runtime.worker_sandbox`); the daemon itself never runs in a container.

You do **not** need the gitlawb `gl` CLI. Identities are native Ed25519 keys created with `dack keygen`.

## Build

```sh
git clone <this-repo> dack-engine && cd dack-engine
make build        # cargo build --release + bun install in openclaude-bridge/
```

`make build` compiles the `dack` binary (`target/release/dack`) and installs the bridge's dependencies
(`@gitlawb/openclaude` + `grammy`, from the public npm registry). By hand:

```sh
cargo build --release
cd openclaude-bridge && bun install --frozen-lockfile && cd ..
```

Confirm the build with the offline test suite:

```sh
cargo test
```

The binary is `target/release/dack`. Put it on your `PATH` (or use `./target/release/dack`); this guide
writes it as `dack`.

## Create your identities

An identity is just an Ed25519 keypair; its public half is a self-certifying `did:key`. Generate the
**operator** identity (the one whose signature makes `dack say` trusted):

```sh
dack keygen --role operator --dir identities/operator
# → prints  did:key: did:key:z6Mk…   (also written to identities/operator/identity.pem, mode 0600)
```

Copy the printed `did:key` — it becomes `operator_did` in your config. The `identities/` dir is
gitignored; **never commit or forward a private key**. (You can also generate `--role soul` and
`--role builder` keys now; the soul key signs/attributes the soul repo's commits. All three are
optional for a first local run.)

> Already manage keys with the gitlawb `gl` CLI? Set `identities: { backend: gitlawb }` and dack will
> shell out to `gl` instead — the native backend derives the identical `did:key` from the same
> `identity.pem`, so switching is a no-op.

## Get a soul

The agent's identity, prompts, memory, and duties live in a **soul** repo. Start from the public
template and make it your own:

```sh
git clone https://github.com/obraztsov/dack-soul.git my-soul
cd my-soul && git remote remove origin && cd ..   # detach from the template; add your own remote later
```

`my-soul/` ships a `SOUL.md`, the state-prompts, and example duties. Edit `SOUL.md` to describe your
agent. For a first run you need no channels or capabilities — the bundled prompts already handle an
operator instruction and a periodic self-prompt. (To back up your soul to your own private repo later,
see `soul_remotes` in [operations](operations.md).)

## Minimal config

Copy the example and fill in the three things without a default — your operator DID, the soul path, and
the model endpoint:

```sh
cp dack.config.example.yaml dack.config.yaml
```

```yaml
operator_did: "did:key:z6Mk…"        # the DID dack keygen printed above
soul_repo: "my-soul"
identities:
  operator: "identities/operator"     # so `dack say` can sign
runtime:
  connector:
    type: opengateway
    api_url: "https://your-gateway/v1"
    api_key: "…"                      # gitignored config only
  model: "your-model-id"
```

`dack.config.yaml` is the operator control plane — keep it **gitignored** (it holds your model key).
Every field other than `operator_did` has a safe default; see the [configuration
reference](configuration.md).

## Run it

```sh
dack run        # or: make run
```

You'll see the harness boot its loops: the consciousness loop (queue → Perceive → wall → Express), the
ingestion loop (cron + webhook → queue), the modules supervisor (if you configured channels), and the
Reflect scheduler. It then sits idle, waiting for a stimulus.

## Talk to it

In another terminal, hand the running agent an instruction:

```sh
dack say "Introduce yourself in one line."
```

This signs the instruction with your operator key and enqueues a trusted `operator_signed` stimulus;
the daemon verifies the signature (against `operator_did`) before honoring it. Watch it work:

```sh
dack status          # alive? queue depth? machinery + secret health?
dack log --follow    # tail the runlog (the agent "syslog")
```

The runlog records every cycle — the model's thought, the tools it called, and the wall's allow/deny —
under the soul's `runlogs/` directory.

## Next steps

- [Configuration reference](configuration.md) — every config field.
- [Concepts](concepts.md) — the states + trust model that bound what the agent can do.
- [Secrets & sandbox](secrets-and-sandbox.md) — the exact secret file formats (X OAuth, tokens).
- [Authoring souls](authoring-souls.md) — write your own prompts and duties.
- [Capabilities](capabilities.md) — give the agent tools (MCP servers).
- [Channels](channels.md) — connect Telegram or another live channel.
- [Operations](operations.md) — running as a daemon, soul-repo push targets, identities.
