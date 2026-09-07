//! The sensor contract — **the gut-feeling fix: sensors perceive, never act.**
//!
//! A sensor is an executable whose *sole job is to perceive*: read the outside world
//! and emit candidate stimulus rows. The contract:
//!   - **Input:**  trigger payload on **stdin** (webhook body; empty for pure cron).
//!                 Harness injects only the **read-scoped env** the sensor declares.
//!   - **Output:** **newline-delimited JSON** candidates on **stdout**.
//!   - **Status:** exit code (0 = success; non-zero → logged failure, no rows).
//!
//! **The hard invariant (enforced, not assumed):** a sensor may only *read*. It must
//! never act, never authenticate as the duck, never hold write/posting/wallet
//! credentials, never mutate state. Test an implementer can apply: *if a sensor needs a
//! write-credential or any signing key, it is not a sensor — it is an action wearing a
//! sensor's clothes, and it is wrong.*
//!
//! Sensor scripts are **harness-run (push), before the agent wakes** → gated by
//! **trust tier**, not agent tool-permission. Honored ONLY for `operator_signed`/`self`
//! provenance and ONLY from trusted repo dirs; for `public`-tier stimuli, frontmatter
//! is data-only and any script reference is ignored — **a tweet cannot ship a script**
//! 

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::error::Result;
use crate::model::stimulus::{StimulusType, TrustTier};

/// One candidate row a sensor emits on stdout. `payload_tier` is optional and may only
/// LOWER trust: the bus clamps it down to the source-derived seed; absent → the seed.
#[derive(Debug, Clone, Deserialize)]
pub struct SensorCandidate {
    #[serde(rename = "type")]
    pub type_: StimulusType,
    pub payload: serde_json::Value,
    #[serde(default)]
    pub dedup_key: Option<String>,
    #[serde(default)]
    pub payload_tier: Option<TrustTier>,
}

/// Whether a stimulus's script reference is honored, by the directive's trust tier.
/// The trusted-dir check is the caller's (the registry only registers
/// defs from trusted repo dirs); this is the tier half.
pub fn scripts_honored(directive_tier: &TrustTier) -> bool {
    *directive_tier == TrustTier::operator() || *directive_tier == TrustTier::self_()
}

#[async_trait]
pub trait SensorRunner: Send + Sync {
    /// Run `exe` with `stdin` piped in and `env` injected (read-scoped only), bounded by
    /// `timeout`. Parse stdout as newline-delimited JSON candidates. Non-zero exit →
    /// `Err` (logged failure, no rows —).
    async fn run(
        &self,
        exe: &Path,
        stdin: &[u8],
        env: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<Vec<SensorCandidate>>;
}

mod subprocess;
pub use subprocess::SubprocessSensor;
