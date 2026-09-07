//! Operator CLI — the v1 operator interface (NOT a web dashboard; ).
//! The operator's real needs are few and map to a CLI + a tailable log.
//!
//! `dack say` is the operator→agent channel: it writes a `Stimulus` row with
//! `operator_signed` tier, signed by the operator DID — so the trust model is uniform
//! (operator instructions are *just* the highest-trust stimulus, not a special path).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use clap::{Parser, Subcommand};
use tokio::sync::mpsc;

use crate::bus::Bus;
use crate::config::DackConfig;
use crate::error::{DackError, Result};
use crate::harness::ingest::{spawn_stimuli_watcher, Ingestor};
use crate::harness::modules::ModuleSupervisor;
use crate::harness::Harness;
use crate::sandbox::{DockerSandbox, HostSandbox, IsolationPolicy, NetworkPolicy, Sandbox};
use crate::config::IdentityBackend;
use crate::identity::gitlawb::GitlawbIdentity;
use crate::identity::local::LocalIdentity;
use crate::identity::{IdentityProvider, IdentityRole};
use crate::model::stimulus::{Priority, Stimulus, StimulusId, StimulusStatus, StimulusType, TrustTier};
use crate::queue::{Queue, SqliteQueue};
use crate::repo::multi::{MultiRemoteRepo, PushTarget};
use crate::repo::{CommitMeta, RepoHost, RepoPath};
use crate::runlog::{DailyFileRunLog, RunLogWriter};
use crate::runtime::openclaude::{OpenClaudeClient, WorkerBackend};
use crate::runtime::RuntimeClient;
use crate::sensor::{SensorRunner, SubprocessSensor};
use crate::sources::{CronScheduler, CronWheel};
use crate::stimuli::Registry;
use crate::webserver::{AxumWebhookListener, WebhookListener};

#[derive(Parser, Debug)]
#[command(name = "dack", about = "DACK actor-scheduler harness")]
pub struct Cli {
    /// Path to the operator config.
    #[arg(long, default_value = "dack.config.yaml")]
    pub config: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Boot the harness and process stimuli (the long-running actor-scheduler).
    Run,
    /// Is it alive, last run, queue depth, current state.
    Status,
    /// Syslog-style tail of runlogs (the "agent syslog").
    Log {
        #[arg(long)]
        follow: bool,
    },
    /// Inject an `operator_signed` stimulus (trusted) into the DB.
    Say { instruction: String },
    /// Halt dispatch (soft kill-switch).
    Pause,
    /// Resume dispatch.
    Resume,
    /// Stop processes (hard).
    Kill,
    /// Force a Reflect run now (an explicit operator override of the scheduled cadence).
    ReflectNow,
    /// Commit external/hand edits to the soul tree (memory/prompts/stimuli) as the operator, so
    /// the daemon's per-run integrity tripwire doesn't revert them.
    Reconcile,
    /// Generate a native Ed25519 identity keypair (`<dir>/identity.pem`, mode 0600) and print its
    /// `did:key`. No external tools — this is how a fresh deploy creates the operator/soul/builder
    /// DIDs. Refuses to overwrite an existing key.
    Keygen {
        /// Role label (operator | soul | builder) — used only in the printed guidance.
        #[arg(long, default_value = "operator")]
        role: String,
        /// Directory to write `identity.pem` into (e.g. `identities/operator`).
        #[arg(long)]
        dir: String,
    },
}

/// Dispatch a parsed CLI command. `run`/`status`/`log` are the daemon + read surfaces; `say`/
/// `pause`/`resume`/`reflect-now`/`reconcile` are the operator controls that act on the shared
/// SQLite queue (cursor flags + injected stimuli) the running daemon observes.
pub async fn dispatch(cli: Cli) -> Result<()> {
    // Structured logging for EVERY command (so an operator command like `reconcile` shows its push
    // results, not just the long-running `run`). Best-effort config load for the level/format; a bad
    // config path still lets the command run and report its own error. Idempotent (`run` won't re-init).
    let log = DackConfig::load(&cli.config).map(|c| c.log).unwrap_or_default();
    crate::logging::init(&log);
    match cli.command {
        Command::Run => run(&cli.config).await,
        Command::Status => status(&cli.config).await,
        Command::Log { follow } => log_cmd(&cli.config, follow).await,
        Command::Say { instruction } => say(&cli.config, instruction).await,
        Command::Pause => set_paused(&cli.config, true).await,
        Command::Resume => set_paused(&cli.config, false).await,
        // The hard stop is SIGTERM (graceful drain) — `systemctl stop dack` / Ctrl-C. A pidfile
        // kill buys nothing over the signal the daemon already drains on cleanly.
        Command::Kill => Err(DackError::NotImplemented(
            "dack kill — use SIGTERM (systemctl stop / Ctrl-C) for a graceful drain",
        )),
        Command::ReflectNow => reflect_now(&cli.config).await,
        Command::Reconcile => reconcile(&cli.config).await,
        Command::Keygen { role, dir } => keygen(&role, &dir),
    }
}

/// `dack keygen --role <r> --dir <d>` — generate a native Ed25519 identity (no `gl`, no gitlawb).
/// Writes `<dir>/identity.pem` (0600) and prints the `did:key` to paste into `dack.config.yaml`.
fn keygen(role: &str, dir: &str) -> Result<()> {
    let did = crate::identity::local::generate(std::path::Path::new(dir))?;
    println!("dack: generated {role} identity → {dir}/identity.pem (mode 0600, never commit it)");
    println!("  did:key: {}", did.0);
    if role == "operator" {
        println!("  → set `operator_did: \"{}\"` in dack.config.yaml,", did.0);
        println!("    and `identities: {{ operator: \"{dir}\" }}`.");
    } else {
        println!("  → set `identities: {{ {role}: \"{dir}\" }}` in dack.config.yaml.");
    }
    Ok(())
}

/// Build the configured identity provider. Native `LocalIdentity` (default) needs no external tools;
/// `GitlawbIdentity` (legacy) shells out to the `gl` CLI. Both derive the same `did:key` from the
/// same `identity.pem`, so switching backends never changes an actor's DID.
async fn build_identity(config: &DackConfig) -> Result<Arc<dyn IdentityProvider>> {
    let dirs = config.identity_dirs();
    match config.identities.backend {
        IdentityBackend::Local => Ok(Arc::new(LocalIdentity::resolve(dirs)?)),
        IdentityBackend::Gitlawb => Ok(Arc::new(GitlawbIdentity::resolve("gl", dirs).await?)),
    }
}

/// `dack log` — the "agent syslog": print today's runlog, and with `--follow` stream
/// new entries as the duck writes them (naive size-poll; the runlog is append-only).
async fn log_cmd(config_path: &str, follow: bool) -> Result<()> {
    use std::io::Write;
    let config = DackConfig::load(config_path)?;
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let path = PathBuf::from(&config.soul_repo).join(format!("runlogs/{date}.md"));
    let mut shown = 0usize;
    loop {
        let text = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        if text.len() > shown {
            print!("{}", &text[shown..]);
            std::io::stdout().flush().ok();
            shown = text.len();
        }
        if !follow {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    }
    Ok(())
}

/// `dack say` — the operator→agent channel. Signs the instruction with the operator
/// key and enqueues it as an `operator_signed`-CLAIMING stimulus; the running daemon VERIFIES the
/// signature at dispatch before honoring the tier — a self-asserted label is never
/// trusted. The operator key signs HERE; the daemon needs only the public operator DID to verify,
/// so the soul/operator private keys never have to meet.
async fn say(config_path: &str, instruction: String) -> Result<()> {
    let config = DackConfig::load(config_path)?;
    let identity = build_identity(&config).await?;
    if identity.did(IdentityRole::Operator).is_none() {
        return Err(DackError::Config(
            "dack say: no operator identity dir (identities.operator) — cannot sign".into(),
        ));
    }
    let sig = identity
        .sign(IdentityRole::Operator, instruction.as_bytes())
        .await?;
    let sig_b64 = String::from_utf8_lossy(&sig.0).trim().to_string();

    let now = chrono::Utc::now().timestamp();
    let stim = Stimulus {
        id: StimulusId(format!("say-{now}")),
        source: "operator-say".into(),
        type_: StimulusType::from("operator_instruction"),
        // CLAIMED operator_signed — the daemon re-derives the REAL tier by verifying the signature.
        directive_tier: TrustTier::operator(),
        payload_tier: TrustTier::self_(),
        payload: serde_json::json!({ "instruction": instruction }),
        provenance: Some(format!("operator_sig:{sig_b64}")),
        received_at: now,
        dedup_key: None,
        pop_after: None,
        // The operator's word is the highest-trust stimulus — let it jump the queue.
        priority: Priority::Urgent,
        status: StimulusStatus::Pending,
        attempts: 0,
        directive_body: instruction,
        entry: config.default_entry.clone(),
    };
    SqliteQueue::open(&config.db_path)?.enqueue(stim).await?;
    println!("dack: operator instruction enqueued (signed; the daemon verifies on dispatch).");
    Ok(())
}

/// `dack pause` / `dack resume` — the soft kill-switch. A shared flag in the queue
/// `cursor` table (the daemon and the CLI open the same SQLite); the consciousness loop checks it
/// at each cycle boundary and idles while set. An in-flight dispatch always finishes first.
async fn set_paused(config_path: &str, paused: bool) -> Result<()> {
    let config = DackConfig::load(config_path)?;
    let queue = SqliteQueue::open(&config.db_path)?;
    queue.set_cursor("paused", if paused { "1" } else { "" }).await?;
    println!(
        "dack: dispatch {} (in-flight runs finish; the loop {} new stimuli).",
        if paused { "PAUSED" } else { "RESUMED" },
        if paused { "stops popping" } else { "resumes popping" }
    );
    Ok(())
}

/// `dack reflect-now` — force a Reflect run now, an explicit operator override of
/// the scheduled cadence. Enqueues the same harness-entered Reflect stimulus the nightly schedule
/// would; entry-Reflect is not rate-limited (the rate-limit guards the *cadence*, which the operator
/// is deliberately overriding here). The running daemon picks it up single-flight.
async fn reflect_now(config_path: &str) -> Result<()> {
    let config = DackConfig::load(config_path)?;
    let now = chrono::Utc::now().timestamp();
    SqliteQueue::open(&config.db_path)?
        .enqueue(crate::harness::reflect_stimulus(now))
        .await?;
    println!("dack: Reflect run enqueued (the daemon will pick it up).");
    Ok(())
}

/// Build the soul repo-host from `effective_soul_remotes()`: ONE local working tree (all
/// reads/writes/commits) fanned out to every configured push target. A `gitlawb://` entry pushes a
/// signed ref-update via its named identity key; every other URL is a plain `git push` (GitHub /
/// GitLab / Gitea / self-hosted / local), optionally with an HTTPS token from the daemon env. A
/// gitlawb entry whose signing identity dir is missing is skipped with a warning (it can't sign
/// without the key) rather than aborting boot; an empty list ⇒ local-only (commits, no push).
/// Convert soul-remote specs into push targets (shared by the soul repo and the runlog repo). A
/// gitlawb entry whose signing identity isn't configured is skipped (logged), so it never blocks boot.
fn remotes_to_targets(
    remotes: &[crate::config::SoulRemote],
    config: &DackConfig,
) -> Vec<PushTarget> {
    use crate::config::RemoteKind;
    let mut targets: Vec<PushTarget> = Vec::new();
    for r in remotes {
        let is_gitlawb = match r.kind {
            Some(RemoteKind::Gitlawb) => true,
            Some(RemoteKind::Git) => false,
            None => r.url.starts_with("gitlawb://"),
        };
        let name = remote_label(&r.url);
        if is_gitlawb {
            let role = r.identity.as_deref().unwrap_or("soul");
            let Some(dir) = config.identities.dir(role) else {
                tracing::warn!("remote `{name}` (gitlawb) needs identities.{role} to sign — skipping");
                continue;
            };
            // Absolutize: `GITLAWB_KEY` is read by the push helper running with cwd = the repo,
            // so a relative dir would not resolve (→ unsigned / 401).
            let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir));
            targets.push(PushTarget::Gitlawb {
                name,
                url: r.url.clone(),
                key_path: dir.join("identity.pem"),
                node: r.node.clone().unwrap_or_else(|| config.gitlawb_node.clone()),
                required: r.required,
            });
        } else {
            targets.push(PushTarget::Git {
                name,
                url: r.url.clone(),
                token: r.auth.as_ref().map(|a| (a.username.clone(), a.token_env.clone())),
                required: r.required,
            });
        }
    }
    targets
}

fn build_soul_repo(
    config: &DackConfig,
    repo_root: &std::path::Path,
    author_did: &str,
) -> Arc<dyn RepoHost> {
    let targets = remotes_to_targets(&config.effective_soul_remotes(), config);
    if targets.is_empty() {
        tracing::info!("no soul_remotes — plain-git, local-only (commits stay on the box, no push)");
    } else {
        tracing::info!(
            "soul push targets: {}",
            targets.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ")
        );
    }
    Arc::new(MultiRemoteRepo::new(
        repo_root.to_path_buf(),
        author_did.to_string(),
        targets,
    ))
}

/// The RUNLOG repo — rooted at `<soul>/runlogs/`, its OWN git repo (the soul gitignores `runlogs/`), so
/// chat detail + telegram handles never reach the public soul. Local-only unless `runlog_remote` is set
/// (then also pushed there — a private backup). Mirror of `build_soul_repo` with the single optional remote.
fn build_runlog_repo(
    config: &DackConfig,
    runlog_root: &std::path::Path,
    author_did: &str,
) -> Arc<dyn RepoHost> {
    let remotes: &[crate::config::SoulRemote] = match &config.runlog_remote {
        Some(r) => std::slice::from_ref(r),
        None => &[],
    };
    let targets = remotes_to_targets(remotes, config);
    if targets.is_empty() {
        tracing::info!("runlog repo local-only (no runlog_remote) — private + disposable");
    } else {
        tracing::info!(
            "runlog push target: {}",
            targets.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ")
        );
    }
    Arc::new(MultiRemoteRepo::new(
        runlog_root.to_path_buf(),
        author_did.to_string(),
        targets,
    ))
}

/// Ensure the runlog repo at `runlog_root` is a healthy git repo before we use it. Healthy (`.git`
/// present + `git status` succeeds) ⇒ no-op. Missing or **corrupt** ⇒ discard the local state entirely
/// (`rm -rf`) and restore-or-fresh: clone from `runlog_remote` if set (the durable backup), else `git
/// init` a fresh EMPTY repo. Boot never tries to salvage whatever loose files are there — the remote is
/// the source of truth, the local repo is disposable. (Today's valid runlogs are preserved by the
/// one-time manual migration that hand-inits this repo; there's no remote to clone from yet.)
fn ensure_runlog_repo(runlog_root: &std::path::Path, runlog_remote: Option<&crate::config::SoulRemote>) {
    let git = |args: &[&str], cwd: &std::path::Path| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    let healthy = runlog_root.join(".git").exists() && git(&["status", "--porcelain"], runlog_root);
    if healthy {
        return;
    }
    tracing::warn!(
        "runlog repo at {} missing/corrupt — discarding local state + {}",
        runlog_root.display(),
        if runlog_remote.is_some() { "cloning from remote" } else { "init fresh empty" }
    );
    let _ = std::fs::remove_dir_all(runlog_root);
    if let Some(r) = runlog_remote {
        // Clone the durable backup. SSH (`git@…`) authenticates with the ambient key; an HTTPS+token
        // remote would need credential injection (TODO when such a remote is wired). gitlawb:// isn't
        // plain-cloneable → falls through to a fresh repo.
        let cloneable = !r.url.starts_with("gitlawb://");
        if cloneable
            && std::process::Command::new("git")
                .args(["clone", "-q", &r.url])
                .arg(runlog_root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        {
            tracing::info!("runlog repo restored from {}", remote_label(&r.url));
            return;
        }
        tracing::warn!("runlog clone unavailable — starting a fresh empty repo");
    }
    let _ = std::fs::create_dir_all(runlog_root);
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.name", "dack"],
        vec!["config", "user.email", "dack@local"],
    ] {
        git(&args, runlog_root);
    }
}

/// A short, human label for a remote URL (logs only): the host for `https`/`ssh`/`git@`,
/// `gitlawb:<repo>` for a `gitlawb://` URL, else the URL itself.
fn remote_label(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("gitlawb://") {
        return rest
            .rsplit('/')
            .next()
            .map(|s| format!("gitlawb:{s}"))
            .unwrap_or_else(|| "gitlawb".into());
    }
    if let Some(rest) = url.strip_prefix("git@") {
        return rest.split(':').next().unwrap_or(rest).to_string();
    }
    for pre in ["https://", "http://", "ssh://"] {
        if let Some(rest) = url.strip_prefix(pre) {
            let host = rest.split('/').next().unwrap_or(rest);
            return host.rsplit('@').next().unwrap_or(host).to_string();
        }
    }
    url.to_string()
}

/// `dack reconcile` — commit external/hand edits to the soul tree
/// (memory/prompts/stimuli) so the daemon's per-run integrity tripwire (`reconcile_soul`, which
/// reverts anything outside the running state's writable dirs) doesn't clobber them. Commits are
/// authored as the **operator DID** (the human shaping the soul, distinct from the duck's Soul-DID
/// Reflect output) and pushed with the soul key (the node credential — author and push are
/// independent: `commit_paths` stamps the author, `push` uses `GITLAWB_KEY`).
async fn reconcile(config_path: &str) -> Result<()> {
    let config = DackConfig::load(config_path)?;
    let repo_root = std::fs::canonicalize(&config.soul_repo)
        .unwrap_or_else(|_| PathBuf::from(&config.soul_repo));
    let author = config.operator_did.clone();
    let repo = build_soul_repo(&config, &repo_root, &author);

    let changes = repo.status().await?;
    if changes.is_empty() {
        println!("dack: soul tree clean — nothing to reconcile.");
        return Ok(());
    }
    let paths: Vec<RepoPath> = changes.iter().map(|c| c.path.clone()).collect();
    let commit = CommitMeta {
        message: format!("reconcile {} external path(s) (operator)", paths.len()),
        author_did: author,
    };
    match repo.commit_paths(&paths, &commit).await? {
        Some(id) => {
            for p in &paths {
                println!("  + {}", p.0);
            }
            println!("dack: committed {} path(s) as operator ({}).", paths.len(), id.0);
            if let Err(e) = repo.push().await {
                tracing::warn!("push failed (kept local; the daemon re-pushes next cycle): {e}");
            }
        }
        None => println!("dack: nothing staged — tree already clean at HEAD."),
    }
    Ok(())
}

/// Boot the ingestion pipeline: cron + webhook → sensor → bus → SQLite queue,
/// with `stimuli/` hot-reload. The consciousness consumer (queue → runtime → wall) lands in
/// until then `run` keeps the duck's senses live and the queue filling.
async fn run(config_path: &str) -> Result<()> {
    let config = Arc::new(DackConfig::load(config_path)?);
    // (Structured logging was already brought up in `dispatch` from this same config.)
    // Fail fast if the capability prefixes overlap (a settle tool must never classify as Post).
    config.validate_capabilities()?;
    // Dry-run (testing): the WALL denies the configured tool prefixes so outward actions are
    // composed-but-not-executed — uniformly across every MCP (twitter, cove, …). No per-server env.
    if config.dry_run.enabled {
        tracing::info!(
            "DRY-RUN — the wall blocks these tool prefixes (composed, not executed): {}",
            config.dry_run.block.join(", ")
        );
    }
    // Absolutize the soul repo so sensor exes (spawned with a changed cwd) and the signed-push
    // `GITLAWB_KEY` resolve regardless of where `dack` is launched from (relative config paths
    // otherwise break under subprocess cwd changes). Falls back to as-written if it doesn't exist.
    let repo_root = std::fs::canonicalize(&config.soul_repo)
        .unwrap_or_else(|_| PathBuf::from(&config.soul_repo));
    // Boot GC: reclaim stale worker workspaces left by crashes / old runs.
    sweep_stale_workspaces(&repo_root);

    let queue: Arc<dyn Queue> = Arc::new(SqliteQueue::open(&config.db_path)?);
    let bus = Arc::new(Bus::new(config.clone(), queue.clone()));
    let registry = Arc::new(RwLock::new(Registry::load(&repo_root)?));
    let sensor: Arc<dyn SensorRunner> = Arc::new(SubprocessSensor::new());

    // One unified FiredTrigger channel drained by the ingestor; cron + webhook both feed it.
    let (tx, rx) = mpsc::channel(256);
    let cron = CronWheel::new(tx.clone());
    let addr: SocketAddr = config
        .webhook_addr
        .parse()
        .map_err(|e| DackError::Config(format!("webhook_addr `{}`: {e}", config.webhook_addr)))?;
    let webhook = AxumWebhookListener::new(addr, tx);

    // Initial schedule + routes from the registry.
    {
        let reg = registry.read().unwrap();
        cron.reschedule(&reg.cron_routes()).await?;
        webhook.set_routes(&reg.webhook_routes()).await?;
        tracing::info!(
            "{} duties registered ({} malformed); webhook on {addr}",
            reg.defs.len(),
            reg.errors.len()
        );
    }

    // Spawn the sources + the hot-reload watcher (kept alive for the process lifetime).
    tokio::spawn(cron.clone().run());
    tokio::spawn(webhook.clone().serve());
    let _watcher =
        spawn_stimuli_watcher(repo_root.clone(), registry.clone(), cron.clone(), webhook.clone())?;

    // ── Consciousness loop: pop the queue → invoke states → the wall ──
    // Shares the queue with ingestion (senses write, cognition reads). The runtime spawns
    // the bun bridge per invocation; if it's unreachable, dispatch errors are logged and the
    // loop stays up (logging-not-rollback).
    // The runtime extensibility point: only openclaude+opengateway is wired; any other engine or
    // connector is a clear `NotImplemented` (the config slot exists, the adapter doesn't yet).
    let runtime: Arc<dyn RuntimeClient> = build_runtime(&config)?;
    // Identity: resolve the configured role dirs via the selected backend (native `local` by
    // default; `gitlawb` shells to `gl`). The Soul dir attributes soul commits (+ signs a
    // `gitlawb://` push, if any); its key never enters agent env.
    let identity: Arc<dyn IdentityProvider> = build_identity(&config).await?;
    let soul_did = identity
        .did(IdentityRole::Soul)
        .map(|d| d.0.clone())
        .unwrap_or_else(|| config.operator_did.clone());

    // Repo-host: ONE local soul working tree fanned out to every configured push target
    // (`soul_remotes`) — signed `gitlawb://` pushes alongside plain-git primaries (GitHub /
    // self-hosted). The Soul DID attributes the commit history; a gitlawb push adds cryptographic
    // provenance. Best-effort per target unless `required`, so a down mirror never blocks a cycle.
    let repo = build_soul_repo(&config, &repo_root, &soul_did);
    // Runlogs (chat detail + telegram handles) live in their OWN private git repo at `<soul>/runlogs/`,
    // NEVER the public soul. `runlogs/` MUST be gitignored in the soul or the soul-integrity tripwire
    // would commit/revert them every cycle — hard-fail rather than silently leak or clobber.
    let soul_gitignore = repo_root.join(".gitignore");
    let runlogs_ignored = std::fs::read_to_string(&soul_gitignore)
        .map(|s| s.lines().any(|l| l.trim().trim_end_matches('/') == "runlogs"))
        .unwrap_or(false);
    if !runlogs_ignored {
        return Err(DackError::Config(format!(
            "`runlogs/` must be gitignored in {} — runlogs are a separate private repo; the public soul must not track them",
            soul_gitignore.display()
        )));
    }
    let runlog_root = repo_root.join("runlogs");
    ensure_runlog_repo(&runlog_root, config.runlog_remote.as_ref());
    let runlog_repo = build_runlog_repo(&config, &runlog_root, &soul_did);
    let runlog: Arc<dyn RunLogWriter> =
        Arc::new(DailyFileRunLog::new(runlog_repo, soul_did.clone()));
    // Rebuild the runlog activity index from the retained day-files (a longer boot is acceptable) so
    // the `environment` map's multi-day histograms are populated from the first wake.
    if let Err(e) = runlog.rebuild_stats().await {
        tracing::warn!("runlog stats rebuild failed (environment histograms start empty): {e}");
    }
    // Shared runtime health registry (the "subconscious"): the broker records per-secret health,
    // the loops record per-stimulus health, and Reflect / `dack status` read the one snapshot.
    let health = Arc::new(crate::health::HealthRegistry::new(chrono::Utc::now().timestamp()));
    // Secrets broker from config — trusted provider scripts; shared by sensors (per-duty
    // `secrets:`) and the Express act phase (per-route `secrets:`). It records each materialize
    // outcome into `health`.
    let broker = Arc::new(crate::secrets::providers::broker_from_config(
        &config,
        health.clone(),
    ));
    let harness = Arc::new(Harness {
        config: config.clone(),
        queue: queue.clone(),
        bus: bus.clone(),
        runtime,
        repo,
        identity,
        runlog,
        broker: broker.clone(),
        sessions: Default::default(),
    });
    // Graceful shutdown: SIGTERM/SIGINT flips the watch; the consciousness loop finishes its
    // in-flight dispatch, then exits (no zombie `dispatched` row). systemd restarts on crash;
    // the next boot reclaims orphans + posts "back online".
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM — draining"),
            _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT — draining"),
        }
        let _ = shutdown_tx.send(true);
    });
    tokio::spawn({
        let h = harness.clone();
        let sd = shutdown_rx.clone();
        async move {
            if let Err(e) = h.run(sd).await {
                tracing::error!("consciousness loop stopped: {e}");
            }
        }
    });
    tracing::info!("consciousness loop up (queue → Perceive → wall → Express).");
    // The scheduled Reflect ticker: enqueues a harness-entered Reflect at `reflect_schedule`
    // (no-op if unset). Harness-owned so it survives `stimuli/` hot-reloads. `dack reflect-now` is the
    // out-of-band manual trigger of the same run.
    tokio::spawn({
        let h = harness.clone();
        let sd = shutdown_rx.clone();
        async move { h.reflect_scheduler(sd).await }
    });

    // Persist the health snapshot to disk every 15s (and once on shutdown) so `dack status` — a
    // SEPARATE process from this daemon — can read the live machinery health (the in-memory registry
    // isn't visible cross-process). The file is the serde snapshot; the reader renders it.
    tokio::spawn({
        let health = health.clone();
        let path = health_path(&config);
        let mut sd = shutdown_rx.clone();
        async move {
            loop {
                write_health_snapshot(&health, &path);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {}
                    _ = sd.changed() => { write_health_snapshot(&health, &path); break; }
                }
            }
        }
    });

    // Modules supervisor (the harness's ownership of long-running channels): spawn + restart-supervise
    // each enabled `modules:` entry (e.g. the Telegram ingress) through the Sandbox seam, injecting
    // its declared secrets. ONE `dack run` now starts the duck's whole runtime — mind + channels —
    // which is the hosted-ducks contract. No-op when `modules:` is empty.
    if !config.modules.is_empty() {
        let sandbox: Arc<dyn Sandbox> = Arc::new(HostSandbox);
        // Default module cwd = the harness process's cwd (the engine working tree that holds
        // `openclaude-bridge/`, `secrets/`, and the adapter configs the module commands reference).
        // A module may override per-entry via `cwd:`.
        let module_cwd = std::env::current_dir().unwrap_or_else(|_| repo_root.clone());
        let supervisor =
            ModuleSupervisor::new(config.modules.clone(), broker.clone(), sandbox, module_cwd);
        let sd = shutdown_rx.clone();
        tokio::spawn(async move { supervisor.run(sd).await });
    }

    // Seed the configured duty roster into the health registry (the "stimulus configuration"), so
    // Reflect / `dack status` see every duty — including a mis-scheduled one that never fires.
    {
        use crate::stimuli::Trigger;
        let reg = registry.read().unwrap();
        let duties = reg
            .defs
            .iter()
            .map(|d| crate::health::DutyInfo {
                id: d.frontmatter.id.clone(),
                trigger: match &d.frontmatter.trigger {
                    Trigger::Cron { schedule } => format!("cron {schedule}"),
                    Trigger::Webhook { path } => format!("webhook {path}"),
                },
                entry: d.frontmatter.entry.clone(),
            })
            .collect();
        health.set_duties(duties);
    }

    let ingestor = Arc::new(Ingestor {
        repo_root,
        config,
        queue,
        bus,
        sensor,
        registry,
        broker,
    });
    tracing::info!("ingestion up (cron+webhook → bus → queue).");
    // The ingestion loop is the process's main future. Stop it on shutdown too, so the whole
    // daemon exits (systemd then restarts it) instead of the consciousness loop alone draining.
    let mut sd = shutdown_rx;
    tokio::select! {
        _ = ingestor.run(rx) => {}
        _ = sd.changed() => tracing::info!("ingestion stopping — shutdown"),
    }
    tracing::info!("stopped cleanly.");
    Ok(())
}

/// Construct the runtime client from `config.runtime` (the engine + connector extensibility point).
/// Only `openclaude`+`opengateway` is wired; any other engine/connector returns a clear
/// `NotImplemented` (the config parses, the adapter just isn't built yet — see `RuntimeConfig`).
fn build_runtime(config: &DackConfig) -> Result<Arc<dyn RuntimeClient>> {
    use crate::config::{Connector, RuntimeEngine};
    let rt = &config.runtime;
    let RuntimeEngine::Openclaude { bridge_dir } = &rt.engine;
    // (engine is exhaustive today — a future `ClaudeCode` arm lands with its own adapter here.)
    if let Connector::Anthropic { .. } = rt.connector {
        return Err(DackError::NotImplemented(
            "runtime.connector: anthropic — not wired yet (only opengateway). \
             The model channel (options.model) is supported; the env/auth wiring is the TODO.",
        ));
    }
    // The bridge env = the forwarded names + the connector's own creds (base URL, key, flags),
    // applied on top so the inline config wins. The provider key reaches the bridge, never the agent.
    let mut env = bridge_env(config);
    for (k, v) in rt.connector.env_overrides() {
        env.insert(k, v);
    }
    let client = OpenClaudeClient::bun_bridge(
        bridge_dir,
        env,
        rt.model.clone(),
        rt.connector.model_via_openai_env(),
        std::time::Duration::from_secs(config.invoke_timeout_secs),
    );
    // a Docker backend for delegated workers (OS isolation) when `worker_sandbox.enabled`.
    let worker = build_worker_backend(config)?;
    Ok(Arc::new(client.with_worker(worker)))
}

/// Build the worker isolation backend from `runtime.worker_sandbox`, with a STARTUP PREFLIGHT. Returns
/// `None` (workers run on host) when disabled, or — when enabled but Docker/the image is unavailable —
/// either errors (`require:true`, refuse to boot so the safety claim holds) or warns + falls back to
/// host (`require:false`). A configured worker policy is locked-down (ro rootfs, caps dropped) EXCEPT
/// `network: Full` (the bridge must egress to the model gateway) and `user: None` (macOS mount friction).
fn build_worker_backend(config: &DackConfig) -> Result<Option<WorkerBackend>> {
    let ws = &config.runtime.worker_sandbox;
    if !ws.enabled {
        return Ok(None);
    }
    if ws.image.trim().is_empty() {
        return Err(DackError::Config(
            "runtime.worker_sandbox.enabled but `image` is empty (build it: docker build -f Dockerfile.worker)".into(),
        ));
    }
    // Precondition: `workspaces/` MUST be gitignored in the soul, or the integrity tripwire reverts
    // (deletes) every worker's output each cycle. Hard-fail rather than silently lose work.
    let gitignore = std::path::Path::new(&config.soul_repo).join(".gitignore");
    let workspaces_ignored = std::fs::read_to_string(&gitignore)
        .map(|s| s.lines().any(|l| l.trim().trim_end_matches('/') == "workspaces"))
        .unwrap_or(false);
    if !workspaces_ignored {
        return Err(DackError::Config(format!(
            "worker_sandbox needs `workspaces/` in {} — else the soul-integrity tripwire reverts worker output",
            gitignore.display()
        )));
    }
    // Preflight Docker + the image (sync; runs once at boot).
    let null = || (std::process::Stdio::null(), std::process::Stdio::null());
    let probe = |args: &[&str]| {
        let (o, e) = null();
        std::process::Command::new("docker").args(args).stdout(o).stderr(e).status().map(|s| s.success()).unwrap_or(false)
    };
    let image_ok = probe(&["version"]) && probe(&["image", "inspect", &ws.image]);
    if !image_ok {
        if ws.require {
            return Err(DackError::Config(format!(
                "worker_sandbox.require: Docker or image `{}` unavailable — refusing to boot (set require:false to fall back to host)",
                ws.image
            )));
        }
        tracing::warn!(
            "worker_sandbox enabled but Docker/image `{}` unavailable — workers FALL BACK TO HOST (require:false).",
            ws.image
        );
        return Ok(None);
    }
    let mut policy = IsolationPolicy::locked_down(ws.image.clone());
    policy.network = NetworkPolicy::Full; // the worker bridge must reach the model gateway
    policy.user = None; // avoid macOS Docker-Desktop uid-mapping write friction on shared mounts
    // A worker runs a FULL agent loop (bun + the SDK + multi-turn context, possibly a `Task`
    // sub-agent in the same process) — the `locked_down` 256m sensor default OOM-kills it ("bridge
    // closed before result"). Default generously; the operator can tighten via `worker_sandbox.memory`.
    policy.memory = ws.memory.clone().or_else(|| Some("2g".to_string()));
    policy.pids_limit = ws.pids_limit;
    tracing::info!("worker_sandbox up — workers OS-isolated in docker image `{}`.", ws.image);
    Ok(Some(WorkerBackend {
        sandbox: Arc::new(DockerSandbox::default()),
        policy,
        command: vec!["bun".into(), "run".into(), "/app/openclaude-bridge/bridge.ts".into()],
        cwd: std::path::PathBuf::from("/workspace"),
    }))
}

/// Boot GC: remove `<soul>/workspaces/*` worker dirs older than ~6h (crash leftovers). Best-effort —
/// the dirs are gitignored + never committed, so this only reclaims disk; recent ones stay inspectable.
fn sweep_stale_workspaces(soul_root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(soul_root.join("workspaces")) else { return };
    let cutoff = std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(6 * 3600));
    let mut swept = 0;
    for e in entries.flatten() {
        let stale = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .zip(cutoff)
            .map(|(m, c)| m < c)
            .unwrap_or(false);
        if stale && e.path().is_dir() && std::fs::remove_dir_all(e.path()).is_ok() {
            swept += 1;
        }
    }
    if swept > 0 {
        tracing::debug!("swept {swept} stale worker workspace(s).");
    }
}

/// Env forwarded into the runtime bridge: `PATH`/`HOME` + the provider/capability names the
/// operator listed (`runtime.env` + `forwarded_env`). Values come from the harness env; the
/// soul key is never among them.
fn bridge_env(config: &DackConfig) -> HashMap<String, String> {
    let mut env = HashMap::new();
    for key in ["PATH", "HOME"] {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.to_string(), v);
        }
    }
    for name in config.runtime.env.iter().chain(config.forwarded_env.iter()) {
        if let Ok(v) = std::env::var(name) {
            env.insert(name.clone(), v);
        }
    }
    env
}

/// Where the daemon persists its health snapshot — co-located with the queue db (which is already
/// gitignored), e.g. `dack.sqlite` → `dack.health.json`.
fn health_path(config: &DackConfig) -> std::path::PathBuf {
    std::path::Path::new(&config.db_path).with_extension("health.json")
}

/// Best-effort atomic write of the health snapshot (tmp + rename). A write failure just means
/// `dack status` shows a slightly-stale or absent view — never fatal to the daemon.
fn write_health_snapshot(health: &crate::health::HealthRegistry, path: &std::path::Path) {
    if let Ok(bytes) = serde_json::to_vec_pretty(&health.snapshot()) {
        let tmp = path.with_extension("tmp");
        let _ = std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, path));
    }
}

/// `dack status` — queue depth + duty registration + the daemon's machinery health (secrets,
/// stimuli, cycle stats) from the periodic snapshot the running daemon writes.
async fn status(config_path: &str) -> Result<()> {
    let config = DackConfig::load(config_path)?;
    let depth = SqliteQueue::open(&config.db_path)?.depth().await?;
    let reg = Registry::load(&config.soul_repo)?;
    println!("queue:  {depth} pending");
    println!(
        "duties: {} registered, {} malformed",
        reg.defs.len(),
        reg.errors.len()
    );
    for (path, err) in &reg.errors {
        println!("  ! {path}: {err}");
    }
    // Machinery health, from the daemon's on-disk snapshot (a separate process — this can't read the
    // daemon's in-memory registry, so it reads the file the daemon writes every ~15s).
    match std::fs::read(health_path(&config)) {
        Ok(bytes) => match serde_json::from_slice::<crate::health::HealthSnapshot>(&bytes) {
            Ok(snap) => println!(
                "\nhealth (from the daemon's last snapshot):\n{}",
                snap.render(chrono::Utc::now().timestamp())
            ),
            Err(e) => println!("\nhealth: snapshot unreadable ({e})"),
        },
        Err(_) => println!("\nhealth: no snapshot yet (is the daemon running?)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_ok(args: &[&str], cwd: &std::path::Path) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn ensure_runlog_repo_inits_noops_and_discards_on_corruption() {
        let dir = std::env::temp_dir().join(format!("dack-runlogrepo-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        // Missing → inits a fresh empty git repo.
        ensure_runlog_repo(&dir, None);
        assert!(dir.join(".git").exists() && git_ok(&["status", "--porcelain"], &dir), "fresh repo");

        // Commit a runlog file.
        std::fs::write(dir.join("2026-06-28.md"), "# runlog\n").unwrap();
        assert!(git_ok(&["add", "-A"], &dir) && git_ok(&["commit", "-q", "-m", "x"], &dir));

        // Healthy → no-op, the committed file is preserved.
        ensure_runlog_repo(&dir, None);
        assert!(dir.join("2026-06-28.md").exists(), "a healthy repo is left untouched");

        // Missing/corrupt .git (no remote) → DISCARD local state entirely + re-init fresh empty;
        // boot does NOT salvage loose files.
        std::fs::remove_dir_all(dir.join(".git")).unwrap();
        ensure_runlog_repo(&dir, None);
        assert!(dir.join(".git").exists() && git_ok(&["status", "--porcelain"], &dir), "re-init'd");
        assert!(
            !dir.join("2026-06-28.md").exists(),
            "corrupt recovery wipes local files (remote is the source of truth, not local salvage)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
