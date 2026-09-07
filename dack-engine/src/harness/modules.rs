//! The "modules" supervisor — the harness's ownership of long-running side processes.
//!
//! A *module* is operator-trusted plumbing declared in `dack.config.yaml` (`modules:`): a channel
//! adapter or companion (e.g. the Telegram ingress) that must run alongside the consciousness loop
//! for the whole process lifetime. The supervisor spawns each enabled module through the
//! [`Sandbox`] seam (so a future deployment can container-isolate it by swapping the backend),
//! injects its declared secrets + static env, and keeps it alive: an exited child is restarted with
//! exponential backoff; a child that ran stably resets the curve. On shutdown every child is killed.
//!
//! This is the single-config contract for the hosted-ducks orchestrator: ONE process (`dack run`)
//! starts the duck's whole runtime — its mind AND its channels. A module touches no trust lattice;
//! it only carries normalized events TO the harness webhook, where the trust contract applies.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::config::ModuleConfig;
use crate::sandbox::{ExecKind, IsolationPolicy, ProcessSpec, Sandbox};
use crate::secrets::providers::SecretsBroker;

/// Restart backoff floor (the first delay after an exit) and ceiling (the cap it doubles up to).
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// A child that stayed up at least this long is "healthy" — its backoff resets to the floor, so a
/// long-lived module that finally dies restarts promptly (not at a previously-escalated delay).
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

/// Owns + supervises the configured long-running modules.
pub struct ModuleSupervisor {
    modules: Vec<ModuleConfig>,
    broker: Arc<SecretsBroker>,
    sandbox: Arc<dyn Sandbox>,
    /// Default working directory for modules that don't pin their own `cwd:` — the harness process's
    /// cwd (the engine working tree holding `openclaude-bridge/`, `secrets/`, adapter configs).
    default_cwd: PathBuf,
}

impl ModuleSupervisor {
    pub fn new(
        modules: Vec<ModuleConfig>,
        broker: Arc<SecretsBroker>,
        sandbox: Arc<dyn Sandbox>,
        default_cwd: PathBuf,
    ) -> Self {
        Self { modules, broker, sandbox, default_cwd }
    }

    /// Spawn a supervisor task per enabled module; resolve when the shutdown watch flips (after the
    /// child tasks have observed it + torn their children down). Disabled modules are logged + skipped.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut handles = Vec::new();
        let mut started = Vec::new();
        for module in self.modules.iter().filter(|m| m.enabled) {
            let cwd = module
                .cwd
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.default_cwd.clone());
            let task = SupervisedModule {
                module: module.clone(),
                broker: self.broker.clone(),
                sandbox: self.sandbox.clone(),
                cwd,
            };
            started.push(module.name.clone());
            handles.push(tokio::spawn(task.supervise(shutdown.clone())));
        }
        let skipped: Vec<&str> =
            self.modules.iter().filter(|m| !m.enabled).map(|m| m.name.as_str()).collect();
        if !skipped.is_empty() {
            tracing::info!("modules disabled (not started): {}", skipped.join(", "));
        }
        if handles.is_empty() {
            return;
        }
        tracing::info!("modules up — supervising {} ({}).", handles.len(), started.join(", "));
        // Block until shutdown; each child task observes the same watch + tears itself down, then
        // we join them so the daemon doesn't exit out from under a still-killing child.
        let _ = shutdown.changed().await;
        for h in handles {
            let _ = h.await;
        }
        tracing::info!("modules stopped.");
    }
}

/// One module's supervision state (the run-restart loop runs against this).
struct SupervisedModule {
    module: ModuleConfig,
    broker: Arc<SecretsBroker>,
    sandbox: Arc<dyn Sandbox>,
    cwd: PathBuf,
}

/// The result of one spawn→wait cycle, telling the loop how to set the next backoff.
enum RunOutcome {
    /// The child exited on its own; `uptime` decides whether the backoff resets.
    Exited { uptime: Duration },
    /// We never got a running child (command build or spawn failed) — back off + retry.
    SpawnFailed,
    /// Shutdown was signalled (child already killed) — stop the loop.
    Shutdown,
}

impl SupervisedModule {
    /// Run-restart loop until shutdown: spawn, wait, back off, repeat.
    async fn supervise(self, mut shutdown: watch::Receiver<bool>) {
        let name = self.module.name.clone();
        let mut backoff = BACKOFF_START;
        loop {
            if *shutdown.borrow() {
                return;
            }
            match self.run_once(&mut shutdown).await {
                RunOutcome::Shutdown => return,
                RunOutcome::Exited { uptime } if uptime >= HEALTHY_UPTIME => {
                    backoff = BACKOFF_START; // healthy run → reset the curve
                }
                RunOutcome::Exited { .. } | RunOutcome::SpawnFailed => {}
            }
            if *shutdown.borrow() {
                return;
            }
            tracing::warn!("module `{name}` restarting in {}s", backoff.as_secs());
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown.changed() => return,
            }
            backoff = (backoff * 2).min(BACKOFF_CAP);
        }
    }

    /// Build the env, spawn the child, wait for it to exit OR for shutdown (killing it on shutdown).
    async fn run_once(&self, shutdown: &mut watch::Receiver<bool>) -> RunOutcome {
        let name = &self.module.name;
        // Module env = static `env:` overlaid with the materialized secrets (resolved fresh each
        // start, so a rotated token is picked up on restart). A secret-resolution failure is
        // non-fatal here — the module starts without it (and likely fails fast, which the backoff
        // loop already handles); we don't want one broken provider to silently stop the channel.
        let mut env = self.module.env.clone();
        match self.broker.env_for(&self.module.secrets).await {
            Ok(secret_env) => env.extend(secret_env),
            Err(e) => tracing::warn!("module `{name}` secrets unresolved ({e}) — starting without"),
        }
        let spec = ProcessSpec {
            command: self.module.command.clone(),
            cwd: self.cwd.clone(),
            env,
            clear_env: false, // operator-trusted: inherit the harness env (PATH/bun) + overlay
            kind: ExecKind::Module,
            mounts: vec![],
            policy: IsolationPolicy::host_passthrough(),
            name: None,
        };
        let mut cmd = match self.sandbox.command(&spec) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("module `{name}` command build failed: {e}");
                return RunOutcome::SpawnFailed;
            }
        };
        cmd.kill_on_drop(true);
        let started = Instant::now();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("module `{name}` spawn failed: {e}");
                return RunOutcome::SpawnFailed;
            }
        };
        tracing::info!("module `{name}` started (pid {:?}).", child.id());
        tokio::select! {
            status = child.wait() => {
                let code = status.ok().and_then(|s| s.code());
                tracing::warn!(
                    "module `{name}` exited (code {code:?}) after {}s.",
                    started.elapsed().as_secs()
                );
                RunOutcome::Exited { uptime: started.elapsed() }
            }
            _ = shutdown.changed() => {
                let _ = child.kill().await;
                tracing::info!("module `{name}` stopped (shutdown).");
                RunOutcome::Shutdown
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::HostSandbox;
    use std::io::Read;

    fn broker() -> Arc<SecretsBroker> {
        Arc::new(SecretsBroker::new_test(vec![]))
    }

    /// A disabled module is never spawned, and `run` returns immediately when nothing is enabled.
    #[tokio::test]
    async fn disabled_modules_are_not_started() {
        let dir = std::env::temp_dir();
        let marker = dir.join(format!("dack-mod-disabled-{}.flag", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let module = ModuleConfig {
            name: "noop".into(),
            // would create the marker if ever spawned
            command: vec!["touch".into(), marker.to_string_lossy().into_owned()],
            secrets: vec![],
            env: Default::default(),
            cwd: None,
            enabled: false,
        };
        let sup = ModuleSupervisor::new(vec![module], broker(), Arc::new(HostSandbox), dir);
        let (_tx, rx) = watch::channel(false);
        // No enabled modules ⇒ returns at once without waiting on shutdown.
        tokio::time::timeout(Duration::from_secs(2), sup.run(rx))
            .await
            .expect("run returned promptly with no enabled modules");
        assert!(!marker.exists(), "disabled module must not be spawned");
    }

    /// A short-lived module is RESTARTED after it exits: a command that appends a byte to a file and
    /// exits should write more than once within the supervision window, then stop on shutdown.
    #[tokio::test]
    async fn exited_module_is_restarted_then_stops_on_shutdown() {
        let dir = std::env::temp_dir();
        let counter = dir.join(format!("dack-mod-restart-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&counter);
        // `sh -c 'printf x >> FILE'` — appends one byte, exits 0. The supervisor restarts it.
        let module = ModuleConfig {
            name: "ticker".into(),
            command: vec![
                "sh".into(),
                "-c".into(),
                format!("printf x >> {}", counter.to_string_lossy()),
            ],
            secrets: vec![],
            env: Default::default(),
            cwd: None,
            enabled: true,
        };
        let sup = ModuleSupervisor::new(vec![module], broker(), Arc::new(HostSandbox), dir);
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(sup.run(rx));
        // First restart waits BACKOFF_START (1s); ~2.5s sees the initial run + at least one restart.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("supervisor stops on shutdown")
            .unwrap();
        let mut s = String::new();
        std::fs::File::open(&counter).unwrap().read_to_string(&mut s).unwrap();
        let _ = std::fs::remove_file(&counter);
        assert!(s.len() >= 2, "module should have run+restarted (got {} runs)", s.len());
    }
}
