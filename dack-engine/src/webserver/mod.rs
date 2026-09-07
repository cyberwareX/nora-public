//! Webhook listener — the inbound HTTP surface for `webhook`-triggered
//! duties. A POST matching a duty's `trigger.path` causes the harness to run that duty's
//! sensor with the request body on stdin, then feed the bus.
//!
//! v1 impl: [`axum_listener::AxumWebhookListener`], bound **localhost-only** alongside the
//! gRPC client (the box exposes nothing public except what a reverse proxy chooses to). A
//! matched POST emits a `FiredTrigger` onto the shared channel the harness drains; the body
//! is always `public`-tier payload — data only, never a script.

use crate::error::Result;

/// Routes inbound webhooks to `FiredTrigger`s on the shared channel. The fired-trigger
/// *output* is the channel (given at construction); the trait carries only hot-reload of
/// the route table — mirroring [`CronScheduler`](crate::sources::CronScheduler).
#[async_trait::async_trait]
pub trait WebhookListener: Send + Sync {
    /// Register/replace the set of `(path, def_id)` routes from the registry (hot-reload).
    async fn set_routes(&self, routes: &[(String, String)]) -> Result<()>;
}

pub mod axum_listener;
pub use axum_listener::AxumWebhookListener;
