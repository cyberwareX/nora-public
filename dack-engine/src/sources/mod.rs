//! Sources — Layer 1 (dumb, pluggable, untrusted). Each source is a
//! small deterministic adapter that turns a trigger into a sensor invocation. **Zero
//! reasoning in any source**. Adding a capability = adding a source.
//!
//! v1 sources: `cron` (timer) and `webhook` (HTTP). The Twitter
//! mention poller is a `cron`-triggered duty whose sensor polls; the push path is the
//! same duty with its trigger flipped to `webhook`.

use crate::error::Result;

/// A fired trigger handed to the harness: which duty fired, plus the raw trigger
/// payload to pass to that duty's sensor on stdin (empty for pure cron).
#[derive(Debug, Clone)]
pub struct FiredTrigger {
    /// The `id` of the `StimulusDef` whose trigger fired.
    pub def_id: String,
    /// Webhook body (empty for cron).
    pub payload: Vec<u8>,
}

/// Schedules cron-triggered duties ("a cron trigger (re)schedules a timer").
/// v1 impl: [`cron::CronWheel`], a `tokio` timer wheel that emits `FiredTrigger`s onto a
/// channel and re-times itself on `reschedule` (hot-reload).
#[async_trait::async_trait]
pub trait CronScheduler: Send + Sync {
    /// (Re)load the schedule from the current registry (hot-reload).
    async fn reschedule(&self, crons: &[(String, String)]) -> Result<()>;
}

pub mod cron;
pub use cron::CronWheel;
