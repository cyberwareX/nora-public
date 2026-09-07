//! Secrets **providers** (interim phase — seam + proposal; see `docs/SECRETS-AND-SANDBOX.md`).
//!
//! Separates "**fetch + rotate** the secret" (the harness's job) from "**use** the secret"
//! (the sensor/skill's job). A provider MATERIALIZES a ready-to-use, short-lived, scoped env
//! — so a sensor reads `X_BEARER_TOKEN` and calls the API, never touching the client secret or
//! refresh token, never persisting rotated tokens. Two wins:
//!   1. **Separation of concerns** — `x_api.py` drops all OAuth2 machinery; it just uses a
//!      bearer the harness handed it.
//!   2. **Blast radius** — arbitrary Reflect-authored sensor code only ever holds a derived,
//!      ~2h, rotatable token; the root credentials (client secret + refresh token) stay in
//!      the harness, never in a sensor's env. Composes with the [`sandbox`](crate::sandbox)
//!      seam, which bounds what that code can *do* with the token.
//!
//! v1 providers: [`StaticEnvProvider`] (passthrough, current behaviour), [`FileEnvProvider`]
//! (a value from a file). [`OAuth2BearerProvider`] (the X case — refresh + persist the
//! rotating token) is the seam's headline and is implemented in the phase, not here.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio::process::Command;

use crate::config::DackConfig;
use crate::error::{DackError, Result};
use crate::health::HealthRegistry;

/// Materializes a usable secret env for a consumer. The provider OWNS fetch + rotation;
/// `materialize` returns currently-valid values, refreshing internally when needed.
#[async_trait]
pub trait SecretsProvider: Send + Sync {
    /// Stable id — how a duty/skill *scopes* which providers feed it (so the X token only
    /// reaches the twitter sensors, not every sensor).
    fn name(&self) -> &str;
    /// The env keys this provider supplies (for declaration + the "no overlap" check).
    fn keys(&self) -> Vec<String>;
    /// Resolve the env to inject right now.
    async fn materialize(&self) -> Result<BTreeMap<String, String>>;
}

/// Passthrough of named host-env vars — today's `forwarded_env` behaviour, as a provider.
/// No rotation; the value is whatever the operator exported.
pub struct StaticEnvProvider {
    name: String,
    keys: Vec<String>,
}

impl StaticEnvProvider {
    pub fn new(name: impl Into<String>, keys: Vec<String>) -> Self {
        Self {
            name: name.into(),
            keys,
        }
    }
}

#[async_trait]
impl SecretsProvider for StaticEnvProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn keys(&self) -> Vec<String> {
        self.keys.clone()
    }
    async fn materialize(&self) -> Result<BTreeMap<String, String>> {
        Ok(self
            .keys
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v)))
            .collect())
    }
}

/// Read a file's contents into a single env var (e.g. a service-account token on disk).
pub struct FileEnvProvider {
    name: String,
    key: String,
    path: std::path::PathBuf,
}

impl FileEnvProvider {
    pub fn new(name: impl Into<String>, key: impl Into<String>, path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            name: name.into(),
            key: key.into(),
            path: path.into(),
        }
    }
}

#[async_trait]
impl SecretsProvider for FileEnvProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn keys(&self) -> Vec<String> {
        vec![self.key.clone()]
    }
    async fn materialize(&self) -> Result<BTreeMap<String, String>> {
        let val = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|e| DackError::Config(format!("secret file {:?}: {e}", self.path)))?;
        Ok(BTreeMap::from([(self.key.clone(), val.trim().to_string())]))
    }
}

/// A trusted, **harness-owned provider script** (e.g. `x_oauth2.py`) — the configurable,
/// no-Rust-change mechanism. The harness runs `command` with `env` (its config: creds-file /
/// token-store paths, never secret values) and reads a JSON object `{"KEY": "value", …}` from
/// stdout. The script owns everything sensitive: fetch, **validity-check (rotate only when the
/// cached token is actually expiring — store a timestamp, don't burn API calls)**, refresh,
/// and persisting the rotated token. So adding X, then cove.trade, then the next thing is a
/// YAML entry + a script — the harness never learns a new secret's shape.
pub struct ScriptSecretsProvider {
    name: String,
    command: Vec<String>,
    env: BTreeMap<String, String>,
    keys: Vec<String>,
}

impl ScriptSecretsProvider {
    pub fn new(
        name: impl Into<String>,
        command: Vec<String>,
        env: BTreeMap<String, String>,
        keys: Vec<String>,
    ) -> Self {
        Self {
            name: name.into(),
            command,
            env,
            keys,
        }
    }
}

#[async_trait]
impl SecretsProvider for ScriptSecretsProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn keys(&self) -> Vec<String> {
        self.keys.clone()
    }
    async fn materialize(&self) -> Result<BTreeMap<String, String>> {
        let (bin, args) = self
            .command
            .split_first()
            .ok_or_else(|| DackError::Config(format!("secrets provider `{}`: empty command", self.name)))?;
        // Trusted but scoped: PATH/HOME (so the interpreter resolves) + the configured refs.
        let mut cmd = Command::new(bin);
        cmd.args(args).env_clear();
        for k in ["PATH", "HOME"] {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }
        cmd.envs(&self.env);
        let out = cmd
            .output()
            .await
            .map_err(|e| DackError::Config(format!("secrets provider `{}` spawn: {e}", self.name)))?;
        if !out.status.success() {
            return Err(DackError::Config(format!(
                "secrets provider `{}` exited {:?}: {}",
                self.name,
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        serde_json::from_slice(&out.stdout).map_err(|e| {
            DackError::Config(format!(
                "secrets provider `{}` output is not a JSON object: {e}",
                self.name
            ))
        })
    }
}

/// Build a [`SecretsBroker`] from the operator's `secrets_providers:` config (all script
/// providers). This is the only place provider *shapes* are known; new secrets are config.
/// `health` is the shared registry the broker records each materialize outcome into.
pub fn broker_from_config(config: &DackConfig, health: Arc<HealthRegistry>) -> SecretsBroker {
    let providers: Vec<Arc<dyn SecretsProvider>> = config
        .secrets_providers
        .iter()
        .map(|p| {
            Arc::new(ScriptSecretsProvider::new(
                p.name.clone(),
                p.command.clone(),
                p.env.clone(),
                p.keys.clone(),
            )) as Arc<dyn SecretsProvider>
        })
        .collect();
    SecretsBroker::new(providers, health)
}

/// Composes providers and materializes the **scoped** env for a consumer. A duty/skill
/// declares the provider names it needs (its `secrets:`); the broker runs exactly those and
/// merges — so secrets are least-privilege per consumer, not a global blob.
///
/// `env_for` is the single choke point every secret-materialization funnels through (sensor
/// ingest, Express MCP auth, module startup), so it is also where per-provider **health** is
/// recorded — one place, and all three consumers inherit it.
pub struct SecretsBroker {
    providers: Vec<Arc<dyn SecretsProvider>>,
    health: Arc<HealthRegistry>,
}

impl SecretsBroker {
    pub fn new(providers: Vec<Arc<dyn SecretsProvider>>, health: Arc<HealthRegistry>) -> Self {
        Self { providers, health }
    }

    /// The shared runtime health registry. The broker holds the canonical handle (it is the one
    /// object every subsystem already reaches via its `broker`), so the ingestion + consciousness
    /// loops record per-stimulus health through here without threading a second field everywhere.
    pub fn health(&self) -> &Arc<HealthRegistry> {
        &self.health
    }

    /// Test-only constructor: a broker with a throwaway health registry (harness tests build
    /// brokers all over and don't assert on health — the production path always injects the
    /// shared registry via [`broker_from_config`]).
    #[cfg(test)]
    pub(crate) fn new_test(providers: Vec<Arc<dyn SecretsProvider>>) -> Self {
        Self::new(providers, Arc::new(HealthRegistry::new(0)))
    }

    /// Merge the env from every provider named in `scopes`. An unknown scope is an error
    /// (fail closed — never silently run a sensor without the secret it declared it needs).
    /// Each provider's success/failure is recorded to the health registry (keyed by name); the
    /// failure message is the provider's own error (e.g. the X breaker's "re-auth needed").
    pub async fn env_for(&self, scopes: &[String]) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for scope in scopes {
            let provider = self
                .providers
                .iter()
                .find(|p| p.name() == scope)
                .ok_or_else(|| DackError::Config(format!("unknown secrets provider scope: {scope}")))?;
            let now = Utc::now().timestamp();
            match provider.materialize().await {
                Ok(env) => {
                    self.health.record_secret_ok(provider.name(), now);
                    out.extend(env);
                }
                Err(e) => {
                    self.health.record_secret_err(provider.name(), &e.to_string(), now);
                    return Err(e);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::HealthRegistry;

    #[tokio::test]
    async fn broker_scopes_to_declared_providers_and_fails_closed() {
        // SAFETY: test-local env var, restored implicitly per-process.
        std::env::set_var("DACK_TEST_TOKEN", "abc");
        let health = Arc::new(HealthRegistry::new(0));
        let broker = SecretsBroker::new(
            vec![Arc::new(StaticEnvProvider::new(
                "twitter",
                vec!["DACK_TEST_TOKEN".into()],
            ))],
            health.clone(),
        );

        // A consumer that declares the `twitter` scope gets exactly its keys.
        let env = broker.env_for(&["twitter".into()]).await.unwrap();
        assert_eq!(env.get("DACK_TEST_TOKEN").map(String::as_str), Some("abc"));
        // A successful materialize is recorded healthy.
        assert!(health.snapshot().secrets["twitter"].is_ok());

        // An undeclared/unknown scope fails closed (never a silent empty env).
        assert!(broker.env_for(&["nope".into()]).await.is_err());
        std::env::remove_var("DACK_TEST_TOKEN");
    }

    #[tokio::test]
    async fn broker_records_provider_failure_in_health() {
        // A provider whose key isn't in the env materializes an empty map (StaticEnvProvider) —
        // so use a script provider that exits non-zero to force a real materialize error.
        let dir = std::env::temp_dir().join(format!("dack-health-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("boom.sh");
        write_exec(&script, "#!/bin/sh\necho 'x: re-auth needed' >&2\nexit 1\n");
        let health = Arc::new(HealthRegistry::new(0));
        let broker = SecretsBroker::new(
            vec![Arc::new(ScriptSecretsProvider::new(
                "x",
                vec!["/bin/sh".into(), script.to_string_lossy().into()],
                BTreeMap::new(),
                vec!["X_BEARER_TOKEN".into()],
            ))],
            health.clone(),
        );

        assert!(broker.env_for(&["x".into()]).await.is_err());
        let h = health.snapshot().secrets["x"].clone();
        assert_eq!(h.consecutive_failures, 1);
        assert!(!h.is_ok());
        assert!(h.last_error.unwrap().contains("re-auth needed"));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn write_exec(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[tokio::test]
    async fn script_provider_runs_a_command_and_parses_json_env() {
        let dir = std::env::temp_dir().join(format!("dack-secp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("prov.sh");
        // A provider that reads its config env and emits a JSON token env (the x_oauth2 shape).
        write_exec(
            &script,
            "#!/bin/sh\nprintf '{\"X_BEARER_TOKEN\":\"tok-%s\"}\\n' \"$WHO\"\n",
        );
        let provider = ScriptSecretsProvider::new(
            "x",
            vec!["/bin/sh".into(), script.to_string_lossy().into()],
            BTreeMap::from([("WHO".into(), "agentdack".into())]),
            vec!["X_BEARER_TOKEN".into()],
        );
        let env = provider.materialize().await.unwrap();
        assert_eq!(env.get("X_BEARER_TOKEN").map(String::as_str), Some("tok-agentdack"));

        // A failing provider is an error, never a silent empty env.
        write_exec(&script, "#!/bin/sh\necho boom >&2\nexit 3\n");
        assert!(provider.materialize().await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
