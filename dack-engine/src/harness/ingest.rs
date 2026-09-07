//! The ingestion pipeline — the **dumb, pre-conscious half** of the harness:
//! sources → sensor → bus → queue. A fired trigger runs the duty's sensor
//! (or, for a pure-cron self-prompt, synthesizes one candidate),
//! and the bus normalizes/coalesces/enqueues. **No reasoning here**; the
//! consciousness loop that *pops* the queue and invokes the runtime is (held).
//!
//! Everything an attacker-influenced byte touches in this file is **data** (sensor stdout,
//! webhook body), never an instruction — the firebreak sits downstream, at Perceive.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::bus::Bus;
use crate::config::DackConfig;
use crate::error::{DackError, Result};
use crate::model::stimulus::StimulusId;
use crate::secrets::providers::SecretsBroker;
use crate::sensor::{SensorCandidate, SensorRunner};
use crate::sources::{CronScheduler, FiredTrigger};
use crate::stimuli::Registry;
use crate::webserver::WebhookListener;

/// Per-sensor wall-clock budget ("bounded by `timeout`").
const SENSOR_TIMEOUT: Duration = Duration::from_secs(30);

/// Owns the ingestion seams. The `registry` is behind an `RwLock` so the hot-reload watcher
/// can swap it under a running drain loop without a restart.
pub struct Ingestor {
    pub repo_root: PathBuf,
    pub config: Arc<DackConfig>,
    pub queue: Arc<dyn crate::queue::Queue>,
    pub bus: Arc<Bus>,
    pub sensor: Arc<dyn SensorRunner>,
    pub registry: Arc<RwLock<Registry>>,
    /// Materializes the short-lived secret env a duty declares (the trusted provider scripts).
    pub broker: Arc<SecretsBroker>,
}

impl Ingestor {
    /// Handle one fired trigger end-to-end. Pure of timers (the caller supplies `now`), so
    /// it is the deterministic seam the acceptance test drives directly.
    pub async fn process(&self, fired: FiredTrigger, now: i64) -> Result<Vec<StimulusId>> {
        // Snapshot the def out of the registry so we never hold the lock across an `.await`.
        let def = {
            let reg = self.registry.read().unwrap();
            reg.get(&fired.def_id).cloned()
        };
        let Some(def) = def else {
            // Duty vanished between firing and handling (a hot-reload race) — drop quietly.
            return Ok(vec![]);
        };

        let candidates = match def.resolved_sensor(&self.repo_root) {
            // A duty with a (trusted) sensor: run it on the trigger payload.
            Some(exe) => {
                // Base read-scoped env + the short-lived token env the duty's declared
                // providers materialize (the harness runs those trusted scripts; the sensor
                // only ever sees the resulting bearer).
                let mut env = self.sensor_env();
                for (k, v) in self.broker.env_for(&def.frontmatter.secrets).await? {
                    env.insert(k, v);
                }
                // Cross-poll dedup: inject the stored watermark so the sensor fetches
                // ONLY items newer than what we've already seen (e.g. X `since_id`). Absent on the
                // first poll. Single-flight makes this read-then-advance race-free.
                if let Some(cur) = &def.frontmatter.cursor {
                    if let Some(since) =
                        self.queue.get_cursor(&cur.key_for(&def.frontmatter.id)).await?
                    {
                        env.insert(cur.env.clone(), since);
                    }
                }
                self.sensor
                    .run(&exe, &fired.payload, &env, SENSOR_TIMEOUT)
                    .await?
            }
            // No sensor: a pure-cron self-prompt (empty payload — the trusted directive body IS the
            // content, attached by the bus) OR a webhook whose POSTED BODY is the payload directly
            // (the channel adapter already normalized it; the body is DATA, never a script).
            // The tier is the source's (webhook → `webhook_trust(path)`, set in `payload_seed`).
            None => {
                let payload = match &def.frontmatter.trigger {
                    crate::stimuli::Trigger::Webhook { .. } => {
                        serde_json::from_slice(&fired.payload).unwrap_or_else(|_| serde_json::json!({}))
                    }
                    crate::stimuli::Trigger::Cron { .. } => serde_json::json!({}),
                };
                // The adapter may include a `dedup_key` (e.g. the telegram message_id) so the same
                // message isn't processed twice.
                let dedup_key = payload.get("dedup_key").and_then(|v| v.as_str()).map(String::from);
                vec![SensorCandidate {
                    type_: def.frontmatter.emits.type_.clone(),
                    payload,
                    dedup_key,
                    payload_tier: None,
                }]
            }
        };

        // Advance the watermark to the newest discovered item, at DISCOVERY time (so a later
        // processing failure never re-discovers it — a mention is "seen" once; we don't risk a
        // double-reply by retrying). Empty poll → no candidate carries the field → no change.
        if let Some(cur) = &def.frontmatter.cursor {
            if let Some(mark) = watermark(&candidates, &cur.field) {
                self.queue
                    .set_cursor(&cur.key_for(&def.frontmatter.id), &mark)
                    .await?;
            }
        }

        // The cycle's PAYLOAD trust seed: derived from the SOURCE (a signed sensor script
        // met with its sensor-secret tiers, a registered webhook tier, or a pure-cron `self`). The
        // bus clamps each candidate's tier down to it (a sensor may only LOWER trust).
        let payload_seed = self.payload_seed(&def).await;
        self.bus.ingest(&def, candidates, now, payload_seed).await
    }

    /// Derive a duty's payload trust seed from its source — provenance seeds
    /// trust, never agent-asserted frontmatter:
    /// - a sensor SCRIPT → `config.script_trust(sha256(source))` (operator-signed, else `public`),
    ///   met with each declared sensor-secret's trust (so a script signed `self` that fetches via a
    ///   `public` secret seeds `public` — code-integrity and data-trust met together);
    /// - a webhook trigger → `config.webhook_trust(path)` (operator-registered, else `public`);
    /// - a pure-cron self-prompt (no sensor) → `self` (the duck's own trusted directive).
    async fn payload_seed(
        &self,
        def: &crate::stimuli::StimulusDef,
    ) -> crate::model::stimulus::TrustTier {
        use crate::model::stimulus::TrustTier;
        use crate::stimuli::Trigger;
        match def.resolved_sensor(&self.repo_root) {
            Some(exe) => {
                let mut t = match tokio::fs::read(&exe).await {
                    Ok(src) => self.config.script_trust(&src),
                    Err(_) => TrustTier::public(), // can't read the source → assume untrusted.
                };
                let lat = self.config.lattice();
                for secret in &def.frontmatter.secrets {
                    t = lat.meet(&t, &self.config.secret_trust(secret));
                }
                t
            }
            None => match &def.frontmatter.trigger {
                Trigger::Webhook { path } => self.config.webhook_trust(path),
                Trigger::Cron { .. } => TrustTier::self_(),
            },
        }
    }

    /// Drain the unified `FiredTrigger` channel forever. A failed duty is a logged line,
    /// never a crash of the loop (logging-not-rollback).
    pub async fn run(self: Arc<Self>, mut rx: mpsc::Receiver<FiredTrigger>) {
        let health = self.broker.health().clone();
        while let Some(fired) = rx.recv().await {
            let def_id = fired.def_id.clone();
            let now = Utc::now().timestamp();
            // Per-duty health: the trigger fired; a failure here is where a dead-secret sensor drop
            // (env_for → Err) is finally *counted*, not just logged — so Reflect/`dack status` see it.
            health.record_stimulus_fired(&def_id, now);
            match self.process(fired, now).await {
                Ok(_) => health.record_stimulus_ok(&def_id, now),
                Err(e) => {
                    tracing::warn!("ingest: duty `{def_id}` failed: {e}");
                    health.record_stimulus_err(&def_id, &e.to_string());
                }
            }
        }
    }

    /// The read-scoped env injected into sensors: only `PATH` (so interpreters
    /// resolve) plus the operator's explicitly `forwarded_env` names. **No soul key** — that
    /// is never forwarded. A name with no value in the harness env is skipped.
    fn sensor_env(&self) -> HashMap<String, String> {
        let mut env = HashMap::new();
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }
        for name in &self.config.forwarded_env {
            if let Ok(v) = std::env::var(name) {
                env.insert(name.clone(), v);
            }
        }
        env
    }
}

/// The max value of `field` across `candidates` — the new cross-poll dedup watermark,
/// or `None` if no candidate carries it. Numeric when both compare as integers (Twitter snowflake
/// ids), else lexical — so a monotonic id advances correctly without assuming a value type.
fn watermark(candidates: &[SensorCandidate], field: &str) -> Option<String> {
    let mut best: Option<String> = None;
    for c in candidates {
        let v = match c.payload.get(field) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => continue,
        };
        if best.as_deref().map_or(true, |b| cursor_gt(&v, b)) {
            best = Some(v);
        }
    }
    best
}

/// Is watermark `a` newer than `b`? Numeric when both parse as integers, else lexical.
fn cursor_gt(a: &str, b: &str) -> bool {
    match (a.parse::<u128>(), b.parse::<u128>()) {
        (Ok(x), Ok(y)) => x > y,
        _ => a > b,
    }
}

/// Reload the registry from `stimuli/` and re-push cron/webhook routes ( hot-reload).
/// The deterministic core the file-watcher calls; also directly testable.
pub async fn reload(
    repo_root: &PathBuf,
    registry: &Arc<RwLock<Registry>>,
    cron: &dyn CronScheduler,
    webhook: &dyn WebhookListener,
) -> Result<()> {
    let reg = Registry::load(repo_root)?;
    let crons = reg.cron_routes();
    let hooks = reg.webhook_routes();
    *registry.write().unwrap() = reg;
    cron.reschedule(&crons).await?;
    webhook.set_routes(&hooks).await?;
    Ok(())
}

/// Watch `<repo_root>/stimuli/` and [`reload`] on any change. Returns the watcher handle —
/// **the caller must keep it alive** (dropping it stops watching). Must be called inside a
/// tokio runtime (it spawns reload tasks).
pub fn spawn_stimuli_watcher(
    repo_root: PathBuf,
    registry: Arc<RwLock<Registry>>,
    cron: Arc<dyn CronScheduler>,
    webhook: Arc<dyn WebhookListener>,
) -> Result<RecommendedWatcher> {
    let handle = tokio::runtime::Handle::current();
    let stimuli = repo_root.join("stimuli");
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_err() {
            return;
        }
        let (repo_root, registry, cron, webhook) =
            (repo_root.clone(), registry.clone(), cron.clone(), webhook.clone());
        handle.spawn(async move {
            if let Err(e) = reload(&repo_root, &registry, cron.as_ref(), webhook.as_ref()).await {
                tracing::warn!("stimuli hot-reload failed: {e}");
            }
        });
    })
    .map_err(|e| DackError::Config(format!("file watcher: {e}")))?;
    watcher
        .watch(&stimuli, RecursiveMode::Recursive)
        .map_err(|e| DackError::Config(format!("watch {stimuli:?}: {e}")))?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::SqliteQueue;
    use crate::sensor::SubprocessSensor;

    fn write_exec(path: &std::path::Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// acceptance: *a cron hook runs a script and registers a deduped stimulus row.*
    /// We drive `process` directly (the cron wheel's firing is proven in `sources::cron`),
    /// so the assertion is deterministic: a real sensor script emits two same-keyed
    /// candidates, and the batch policy folds them into **one** queued row.
    #[tokio::test]
    async fn cron_fire_runs_a_script_and_registers_a_deduped_row() {
        let root = std::env::temp_dir().join(format!("dack-ingest-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();

        write_exec(
            &root.join("stimuli/poller/scripts/emit.sh"),
            "#!/bin/sh\n\
             printf '{\"type\":\"poll_item\",\"payload\":{\"n\":1},\"dedup_key\":\"t1\"}\\n'\n\
             printf '{\"type\":\"poll_item\",\"payload\":{\"n\":2},\"dedup_key\":\"t1\"}\\n'\n",
        );
        std::fs::write(
            root.join("stimuli/poller/STIMULUS.md"),
            "---\nid: poller\ntrigger: { type: cron, schedule: \"* * * * *\" }\n\
             sensor: ./scripts/emit.sh\ndirective_tier: self\n\
             emits:\n  type: poll_item\n  default_payload_tier: public\n\
             coalesce: { mode: batch, window_sec: 600, dedup_key: thread }\n\
             entry: perceive\n---\nPoll directive.\n",
        )
        .unwrap();

        let config = Arc::new(DackConfig::from_yaml("operator_did: \"did:key:zOp\"").unwrap());
        let queue: Arc<dyn crate::queue::Queue> = Arc::new(SqliteQueue::open_in_memory().unwrap());
        let ingestor = Ingestor {
            repo_root: root.clone(),
            config: config.clone(),
            bus: Arc::new(Bus::new(config, queue.clone())),
            queue: queue.clone(),
            sensor: Arc::new(SubprocessSensor::new()),
            broker: Arc::new(SecretsBroker::new_test(vec![])),
            registry: Arc::new(RwLock::new(Registry::load(&root).unwrap())),
        };

        // Simulate the cron fire.
        let ids = ingestor
            .process(FiredTrigger { def_id: "poller".into(), payload: vec![] }, 1000)
            .await
            .expect("sensor ran + bus ingested");

        assert_eq!(ids.len(), 1, "two candidates coalesced into one wake");
        assert_eq!(queue.depth().await.unwrap(), 1, "exactly one deduped row queued");
        let row = queue.next().await.unwrap().unwrap();
        let items = row.payload.get("items").and_then(|v| v.as_array()).unwrap();
        assert_eq!(items.len(), 2, "both polled items folded into the row");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn watermark_is_numeric_then_lexical() {
        let c = |id: &str| SensorCandidate {
            type_: crate::model::stimulus::StimulusType::from("m"),
            payload: serde_json::json!({ "id": id }),
            dedup_key: None,
            payload_tier: None,
        };
        // Numeric max (a snowflake id), NOT lexical: "100" > "99" numerically though "99" wins lexically.
        assert_eq!(watermark(&[c("99"), c("100"), c("250")], "id").as_deref(), Some("250"));
        // Lexical fallback for non-integer values.
        assert_eq!(watermark(&[c("abc"), c("abd")], "id").as_deref(), Some("abd"));
        // Missing field → no watermark.
        assert_eq!(watermark(&[c("1")], "nope"), None);
    }

    /// the harness fetches the stored watermark, injects it into the sensor env, and
    /// advances it from the discovered candidates — so a poll never re-surfaces a handled item.
    #[tokio::test]
    async fn cursor_dedup_injects_and_advances_watermark() {
        let root = std::env::temp_dir().join(format!("dack-cursor-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        // Sensor: emit ONE mention candidate echoing the injected watermark + a fixed id.
        write_exec(
            &root.join("stimuli/mtest/scripts/emit.sh"),
            "#!/bin/sh\nprintf '{\"type\":\"mention\",\"payload\":{\"id\":\"500\",\"saw_since\":\"%s\"}}\\n' \"${DACK_SINCE_ID:-none}\"\n",
        );
        std::fs::write(
            root.join("stimuli/mtest/STIMULUS.md"),
            "---\nid: mtest\ntrigger: { type: cron, schedule: \"* * * * *\" }\n\
             sensor: ./scripts/emit.sh\ndirective_tier: self\n\
             emits:\n  type: mention\n  default_payload_tier: public\n\
             cursor: { field: id, env: DACK_SINCE_ID }\nentry: perceive\n---\nPoll.\n",
        )
        .unwrap();

        let config = Arc::new(DackConfig::from_yaml("operator_did: \"did:key:zOp\"").unwrap());
        let queue: Arc<dyn crate::queue::Queue> = Arc::new(SqliteQueue::open_in_memory().unwrap());
        let ingestor = Ingestor {
            repo_root: root.clone(),
            config: config.clone(),
            bus: Arc::new(Bus::new(config, queue.clone())),
            queue: queue.clone(),
            sensor: Arc::new(SubprocessSensor::new()),
            broker: Arc::new(SecretsBroker::new_test(vec![])),
            registry: Arc::new(RwLock::new(Registry::load(&root).unwrap())),
        };

        // First poll: no watermark yet → sensor sees none; cursor advances to 500.
        ingestor.process(FiredTrigger { def_id: "mtest".into(), payload: vec![] }, 1000).await.unwrap();
        assert_eq!(queue.get_cursor("mtest").await.unwrap().as_deref(), Some("500"));
        let row1 = queue.next().await.unwrap().unwrap();
        assert_eq!(row1.payload.get("saw_since").unwrap(), "none", "first poll injects no watermark");

        // Second poll: the harness fetches the stored 500 and injects it into the sensor env.
        ingestor.process(FiredTrigger { def_id: "mtest".into(), payload: vec![] }, 1001).await.unwrap();
        let row2 = queue.next().await.unwrap().unwrap();
        assert_eq!(row2.payload.get("saw_since").unwrap(), "500", "stored watermark was injected");

        std::fs::remove_dir_all(&root).ok();
    }

    /// An unknown / vanished duty (hot-reload race) is dropped, never a panic.
    #[tokio::test]
    async fn unknown_duty_is_dropped_quietly() {
        let root = std::env::temp_dir().join(format!("dack-ingest-x-{}", std::process::id()));
        let config = Arc::new(DackConfig::from_yaml("operator_did: \"did:key:zOp\"").unwrap());
        let queue: Arc<dyn crate::queue::Queue> = Arc::new(SqliteQueue::open_in_memory().unwrap());
        let ingestor = Ingestor {
            repo_root: root.clone(),
            config: config.clone(),
            bus: Arc::new(Bus::new(config, queue.clone())),
            queue,
            sensor: Arc::new(SubprocessSensor::new()),
            broker: Arc::new(SecretsBroker::new_test(vec![])),
            registry: Arc::new(RwLock::new(Registry::default())),
        };
        let ids = ingestor
            .process(FiredTrigger { def_id: "ghost".into(), payload: vec![] }, 1)
            .await
            .unwrap();
        assert!(ids.is_empty());
    }
}
