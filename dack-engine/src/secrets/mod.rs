//! Secret store + the two-tier env/secret separation.
//!
//! The test for where a secret lives is NOT "sensitive or not" — it is **"if this
//! leaked, can I rotate it / is the loss recoverable?"**
//!   - Recoverable (Twitter key, Bankr key, Builder DID key) → **forwarded env**
//!     (cheap leak = rotation; keeps skills self-contained & portable).
//!   - Continuity-ending (Soul DID key) → **harness-only**, never in agent env; the
//!     harness uses it on the agent's behalf.
//!
//! Sharper corollary: **treat everything agent-reachable as public-by-assumption** —
//! prompt injection can exfiltrate agent context into a tweet at any time. So even a
//! forwarded value should pass "would it be fine as a tweet?" (true for a handle/limit,
//! accepted-as-recoverable for API keys).

use std::collections::HashMap;

use crate::config::DackConfig;
use crate::error::Result;

pub mod providers;

/// Resolves secret *references* (e.g. `file:///run/secrets/soul_did_key`) to values.
/// Implementations must keep harness-only secrets (the Soul DID key) off any path that
/// reaches the agent.
pub trait SecretStore: Send + Sync {
    /// Resolve a harness-only secret by its config reference. NEVER forwarded to env.
    fn resolve(&self, reference: &str) -> Result<Vec<u8>>;
}

/// Build the env map forwarded into the agent + sensor processes. Reads the
/// **names** in `forwarded_env` from the harness process env. The Soul DID key is never
/// in this set — it is a `secrets:` reference, resolved only via [`SecretStore`].
pub fn forwarded_env(config: &DackConfig) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for name in &config.forwarded_env {
        if let Ok(val) = std::env::var(name) {
            out.insert(name.clone(), val);
        }
    }
    out
}

/// Reads `file://` references from disk. The default v1 store.
pub struct FileSecretStore;

impl SecretStore for FileSecretStore {
    fn resolve(&self, reference: &str) -> Result<Vec<u8>> {
        if let Some(path) = reference.strip_prefix("file://") {
            Ok(std::fs::read(path)?)
        } else {
            Err(crate::error::DackError::Config(format!(
                "unsupported secret reference scheme: {reference}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soul_key_is_never_forwarded() {
        let cfg = DackConfig::from_yaml(
            r#"
operator_did: "did:key:zOp"
forwarded_env: [TWITTER_API_KEY]
secrets:
  soul_did_key: "file:///run/secrets/soul_did_key"
"#,
        )
        .unwrap();
        let env = forwarded_env(&cfg);
        // Even if the process happened to export it, it's not in the forwarded set.
        assert!(!env.contains_key("soul_did_key"));
        assert!(!env.contains_key("SOUL_DID_KEY"));
    }
}
