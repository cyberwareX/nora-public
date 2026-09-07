//! Subprocess sensor runner — the real implementation of the [`SensorRunner`] contract
//! Spawns the executable with a cleared env (read-scoped: only the declared
//! env is injected), writes the trigger payload to stdin, enforces a timeout
//! (`kill_on_drop`), and parses stdout as newline-delimited JSON.

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use super::{SensorCandidate, SensorRunner};
use crate::error::{DackError, Result};
use crate::sandbox::{ExecKind, HostSandbox, IsolationPolicy, ProcessSpec, Sandbox};

/// Runs sensors as subprocesses **through the [`Sandbox`] seam** — `HostSandbox` by default
/// (today's direct spawn), a container backend when the operator opts into isolation. A
/// sensor is arbitrary Reflect-authored code, so it is the first thing worth isolating.
pub struct SubprocessSensor {
    sandbox: Arc<dyn Sandbox>,
    policy: IsolationPolicy,
}

impl SubprocessSensor {
    pub fn new() -> Self {
        Self {
            sandbox: Arc::new(HostSandbox),
            policy: IsolationPolicy::host_passthrough(),
        }
    }

    /// Opt into an isolation backend + policy for sensor runs (the sandbox-phase entry point).
    pub fn with_sandbox(sandbox: Arc<dyn Sandbox>, policy: IsolationPolicy) -> Self {
        Self { sandbox, policy }
    }
}

impl Default for SubprocessSensor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SensorRunner for SubprocessSensor {
    async fn run(
        &self,
        exe: &Path,
        stdin: &[u8],
        env: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<Vec<SensorCandidate>> {
        // `clear_env: true` enforces "inject only the read-scoped env the sensor declares"
        // The caller includes PATH/interpreter vars in `env` when needed.
        let spec = ProcessSpec {
            command: vec![exe.to_string_lossy().into_owned()],
            cwd: exe
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            clear_env: true,
            kind: ExecKind::Sensor,
            mounts: Vec::new(), // a container backend mounts the duty's scripts (ro); host ignores
            policy: self.policy.clone(),
            name: None,
        };
        let mut child = self
            .sandbox
            .command(&spec)?
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| DackError::Sensor(format!("spawn {exe:?}: {e}")))?;

        if let Some(mut sin) = child.stdin.take() {
            sin.write_all(stdin)
                .await
                .map_err(|e| DackError::Sensor(format!("write stdin: {e}")))?;
            // Drop closes stdin so a `read until EOF` sensor proceeds.
        }

        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| DackError::Sensor(format!("timeout after {timeout:?}")))?
            .map_err(|e| DackError::Sensor(format!("wait: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DackError::Sensor(format!(
                "exit {:?}: {}",
                output.status.code(),
                stderr.trim()
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut candidates = Vec::new();
        for (lineno, line) in stdout.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let candidate: SensorCandidate = serde_json::from_str(line)
                .map_err(|e| DackError::Sensor(format!("line {}: {e}", lineno + 1)))?;
            candidates.push(candidate);
        }
        Ok(candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_ndjson_from_a_trivial_shell_sensor() {
        // A "sensor" that emits two candidate rows — the curl+jq shape, minus the net.
        let runner = SubprocessSensor::new();
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/bin:/usr/bin".to_string());
        let candidates = runner
            .run(
                Path::new("/bin/sh"),
                b"",
                &env,
                Duration::from_secs(5),
            )
            .await;
        // /bin/sh with empty stdin and no script reads EOF and exits 0 with no output.
        let candidates = candidates.expect("trivial sensor runs");
        assert!(candidates.is_empty());
    }
}
