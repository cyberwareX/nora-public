//! Runtime **health registry** — the harness's honest read of its own machinery (its
//! "subconscious"). Every subsystem writes to one `Arc`-shared registry; three consumers read the
//! same [`HealthSnapshot`]:
//!   1. **Reflect** — a trusted `subconscious-health` block in the duck's self-review prompt, so it
//!      *notices* "my X token is dead" instead of silently degrading.
//!   2. **`dack status`** — the operator's on-demand window (the incident that motivated this was
//!      "no notification was sent").
//!   3. A future **orchestrator API** — [`HealthSnapshot`] is `Serialize`, so exposing it as JSON
//!      later is free.
//!
//! Two kinds of health are tracked:
//!   - **secrets** — recorded at the single choke point [`crate::secrets::SecretsBroker::env_for`],
//!     so a provider failure on ANY path (sensor / MCP auth / module) lands here. The X provider's
//!     circuit-breaker state (dead / cooling-down) arrives as `last_error`.
//!   - **stimuli** — recorded by the ingestion + consciousness loops: did a duty fire, did its
//!     cycle succeed, what was the last error, how many consecutive failures.
//!
//! In-memory, **since-boot** (a restart resets it, but the next materialize re-populates a still-
//! dead secret — so Reflect still sees it). The lock is a plain `Mutex` held only for the tiny
//! synchronous record/snapshot critical sections — never across an `.await`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Health of one secrets provider (keyed by provider name, e.g. `x`, `cove_read`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SecretHealth {
    /// Last time it materialized a token successfully (unix seconds).
    pub last_ok: Option<i64>,
    /// The most recent failure message (the provider script's stderr — carries the X breaker's
    /// "re-auth needed" / "cooling down" reason).
    pub last_error: Option<String>,
    /// Failures in a row since the last success (0 ⇒ healthy).
    pub consecutive_failures: u32,
    /// Last time it was exercised at all (ok or err).
    pub last_checked: Option<i64>,
}

impl SecretHealth {
    /// Healthy = has succeeded and is not currently in a failure streak.
    pub fn is_ok(&self) -> bool {
        self.consecutive_failures == 0 && self.last_ok.is_some()
    }
}

/// Health of one stimulus/duty (keyed by duty id, or a synthetic id like `reflect`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StimulusHealth {
    /// Last time this duty fired / was dispatched (unix seconds).
    pub last_fired: Option<i64>,
    /// Last time a cycle for it completed successfully.
    pub last_ok: Option<i64>,
    /// Total fires since boot.
    pub fire_count: u64,
    /// Total errored cycles/ingests since boot.
    pub error_count: u64,
    /// Errors in a row since the last success (0 ⇒ healthy).
    pub consecutive_failures: u32,
    /// The most recent failure message.
    pub last_error: Option<String>,
}

impl StimulusHealth {
    pub fn is_ok(&self) -> bool {
        self.consecutive_failures == 0
    }
}

/// The *configuration* of one registered duty (seeded from the stimulus registry at boot), so the
/// snapshot self-contains the full roster — a duty that has never fired still shows, which is itself
/// a signal (a mis-scheduled cron that never fires is a bug worth noticing).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DutyInfo {
    pub id: String,
    /// Human trigger description, e.g. `cron 0 4 * * *` or `webhook /telegram/op`.
    pub trigger: String,
    /// The entry state-prompt this duty opens.
    pub entry: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct HealthState {
    /// The configured duty roster (from the registry) — the "stimulus configuration".
    duties: Vec<DutyInfo>,
    secrets: BTreeMap<String, SecretHealth>,
    stimuli: BTreeMap<String, StimulusHealth>,
    cycles_ok: u64,
    cycles_err: u64,
    booted_at: i64,
}

/// A point-in-time, serializable copy of the whole registry (for rendering / the future API).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub duties: Vec<DutyInfo>,
    pub secrets: BTreeMap<String, SecretHealth>,
    pub stimuli: BTreeMap<String, StimulusHealth>,
    pub cycles_ok: u64,
    pub cycles_err: u64,
    pub booted_at: i64,
}

/// The one shared registry. Cheap to `Arc`-clone into every subsystem.
pub struct HealthRegistry {
    inner: Mutex<HealthState>,
}

impl HealthRegistry {
    pub fn new(booted_at: i64) -> Self {
        Self {
            inner: Mutex::new(HealthState {
                booted_at,
                ..Default::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HealthState> {
        // A poisoned lock (a writer panicked mid-record) must not take health down — recover the
        // guard and carry on; the worst case is one slightly-stale counter.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A provider materialized a token — clears its failure streak.
    pub fn record_secret_ok(&self, name: &str, now: i64) {
        let mut st = self.lock();
        let h = st.secrets.entry(name.to_string()).or_default();
        h.last_ok = Some(now);
        h.last_checked = Some(now);
        h.consecutive_failures = 0;
        h.last_error = None;
    }

    /// A provider failed to materialize — bumps its streak, keeps the reason.
    pub fn record_secret_err(&self, name: &str, err: &str, now: i64) {
        let mut st = self.lock();
        let h = st.secrets.entry(name.to_string()).or_default();
        h.last_checked = Some(now);
        h.consecutive_failures = h.consecutive_failures.saturating_add(1);
        h.last_error = Some(err.to_string());
    }

    /// A duty fired / was dispatched.
    pub fn record_stimulus_fired(&self, id: &str, now: i64) {
        let mut st = self.lock();
        let h = st.stimuli.entry(id.to_string()).or_default();
        h.last_fired = Some(now);
        h.fire_count = h.fire_count.saturating_add(1);
    }

    /// A duty's cycle completed cleanly — clears its failure streak.
    pub fn record_stimulus_ok(&self, id: &str, now: i64) {
        let mut st = self.lock();
        let h = st.stimuli.entry(id.to_string()).or_default();
        h.last_ok = Some(now);
        h.consecutive_failures = 0;
        h.last_error = None;
    }

    /// A duty's cycle (or its ingest) errored. Recency comes from the preceding `record_stimulus_fired`
    /// (its `last_fired` is the failing attempt), so this needs no timestamp of its own.
    pub fn record_stimulus_err(&self, id: &str, err: &str) {
        let mut st = self.lock();
        let h = st.stimuli.entry(id.to_string()).or_default();
        h.error_count = h.error_count.saturating_add(1);
        h.consecutive_failures = h.consecutive_failures.saturating_add(1);
        h.last_error = Some(err.to_string());
    }

    /// Seed the configured duty roster (at boot, and on `stimuli/` hot-reload). Replaces the list.
    pub fn set_duties(&self, duties: Vec<DutyInfo>) {
        self.lock().duties = duties;
    }

    /// Coarse consciousness-loop tally (a wake finished ok / errored).
    pub fn record_cycle(&self, ok: bool) {
        let mut st = self.lock();
        if ok {
            st.cycles_ok = st.cycles_ok.saturating_add(1);
        } else {
            st.cycles_err = st.cycles_err.saturating_add(1);
        }
    }

    pub fn snapshot(&self) -> HealthSnapshot {
        let st = self.lock();
        HealthSnapshot {
            duties: st.duties.clone(),
            secrets: st.secrets.clone(),
            stimuli: st.stimuli.clone(),
            cycles_ok: st.cycles_ok,
            cycles_err: st.cycles_err,
            booted_at: st.booted_at,
        }
    }
}

impl HealthSnapshot {
    /// Render a compact human/duck-readable report (shared by the Reflect block and `dack status`).
    /// Green stays terse; only the degraded rows elaborate. `now` is the reference time for the
    /// relative "… ago" / uptime figures.
    pub fn render(&self, now: i64) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "Uptime {}. Cycles since boot: {} ok, {} errored.\n",
            dur(now - self.booted_at),
            self.cycles_ok,
            self.cycles_err
        ));

        // Secrets — one line when all healthy, per-secret detail when any is failing.
        let any_secret_bad = self.secrets.values().any(|h| !h.is_ok());
        if self.secrets.is_empty() {
            s.push_str("\nSecrets: (none configured)\n");
        } else if !any_secret_bad {
            let names: Vec<&str> = self.secrets.keys().map(String::as_str).collect();
            s.push_str(&format!("\nSecrets: all ok — {}\n", names.join(", ")));
        } else {
            s.push_str("\nSecrets:\n");
            for (name, h) in &self.secrets {
                if h.is_ok() {
                    s.push_str(&format!("  - {name}: ok (last ok {})\n", opt_ago(now, h.last_ok)));
                } else {
                    s.push_str(&format!(
                        "  - {name}: FAILING ({} in a row) — last: \"{}\" — last ok {}\n",
                        h.consecutive_failures,
                        h.last_error.as_deref().unwrap_or("?"),
                        opt_ago(now, h.last_ok),
                    ));
                }
            }
        }

        // Stimuli — the full configured duty roster, each annotated with its health.
        s.push_str(&format!("\nStimuli ({} duties):\n", self.duties.len()));
        let mut shown = std::collections::BTreeSet::new();
        for d in &self.duties {
            shown.insert(d.id.as_str());
            let status = match self.stimuli.get(&d.id) {
                None => "never fired since boot".to_string(),
                Some(h) if h.is_ok() => {
                    format!("ok — {} fires, last {}", h.fire_count, opt_ago(now, h.last_fired))
                }
                Some(h) => format!(
                    "FAILING ({} in a row) — last: \"{}\" — last fired {}",
                    h.consecutive_failures,
                    h.last_error.as_deref().unwrap_or("?"),
                    opt_ago(now, h.last_fired),
                ),
            };
            s.push_str(&format!("  - {} [{} → {}]: {}\n", d.id, d.trigger, d.entry, status));
        }
        // Synthesized stimuli not in the duty roster (reflect, back_online, workers).
        for (id, h) in &self.stimuli {
            if shown.contains(id.as_str()) {
                continue;
            }
            let status = if h.is_ok() {
                format!("ok — last {}", opt_ago(now, h.last_ok.or(h.last_fired)))
            } else {
                format!(
                    "FAILING ({} in a row) — last: \"{}\"",
                    h.consecutive_failures,
                    h.last_error.as_deref().unwrap_or("?")
                )
            };
            s.push_str(&format!("  - {id}: {status}\n"));
        }
        s
    }
}

/// A coarse `Nd Mh` / `Mh Ns` / `Nm` duration string.
fn dur(secs: i64) -> String {
    let s = secs.max(0);
    let (d, h, m) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m")
    }
}

fn ago(now: i64, ts: i64) -> String {
    let d = now - ts;
    if d < 60 {
        "just now".into()
    } else {
        format!("{} ago", dur(d))
    }
}

fn opt_ago(now: i64, ts: Option<i64>) -> String {
    ts.map_or_else(|| "never".into(), |t| ago(now, t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_health_tracks_failure_streak_and_recovery() {
        let reg = HealthRegistry::new(1000);
        reg.record_secret_ok("x", 1000);
        assert!(reg.snapshot().secrets["x"].is_ok());

        reg.record_secret_err("x", "invalid_grant — re-auth needed", 1100);
        reg.record_secret_err("x", "in cooldown 3600s", 1200);
        let h = reg.snapshot().secrets["x"].clone();
        assert_eq!(h.consecutive_failures, 2);
        assert!(!h.is_ok());
        assert_eq!(h.last_error.as_deref(), Some("in cooldown 3600s"));
        assert_eq!(h.last_ok, Some(1000)); // the earlier success is remembered

        reg.record_secret_ok("x", 1300); // re-auth fixed it
        let h = reg.snapshot().secrets["x"].clone();
        assert_eq!(h.consecutive_failures, 0);
        assert!(h.is_ok());
        assert!(h.last_error.is_none());
    }

    #[test]
    fn stimulus_health_counts_fires_and_errors() {
        let reg = HealthRegistry::new(0);
        reg.record_stimulus_fired("twitter-mentions", 10);
        reg.record_stimulus_err("twitter-mentions", "mcp twitter-read: X_BEARER_TOKEN not set");
        reg.record_stimulus_fired("twitter-mentions", 20);
        reg.record_stimulus_err("twitter-mentions", "still dead");
        let h = reg.snapshot().stimuli["twitter-mentions"].clone();
        assert_eq!(h.fire_count, 2);
        assert_eq!(h.error_count, 2);
        assert_eq!(h.consecutive_failures, 2);
        assert!(!h.is_ok());

        reg.record_stimulus_fired("twitter-mentions", 30);
        reg.record_stimulus_ok("twitter-mentions", 31);
        let h = reg.snapshot().stimuli["twitter-mentions"].clone();
        assert_eq!(h.fire_count, 3);
        assert_eq!(h.consecutive_failures, 0); // recovered
        assert!(h.is_ok());
    }

    #[test]
    fn cycle_tally_and_snapshot_serializes() {
        let reg = HealthRegistry::new(42);
        reg.record_cycle(true);
        reg.record_cycle(true);
        reg.record_cycle(false);
        reg.record_secret_ok("cove_read", 5);
        let snap = reg.snapshot();
        assert_eq!(snap.cycles_ok, 2);
        assert_eq!(snap.cycles_err, 1);
        assert_eq!(snap.booted_at, 42);
        // The snapshot is the one shape Reflect / `dack status` / the future API all read.
        let json = serde_json::to_string(&snap).expect("snapshot serializes");
        assert!(json.contains("\"cove_read\""));
        assert!(json.contains("\"cycles_ok\":2"));
    }

    #[test]
    fn render_flags_failures_and_lists_the_whole_roster() {
        let now = 100_000;
        let reg = HealthRegistry::new(now - 3600); // up 1h
        reg.set_duties(vec![
            DutyInfo { id: "twitter-mentions".into(), trigger: "cron */2 * * * *".into(), entry: "perceive".into() },
            DutyInfo { id: "heartbeat".into(), trigger: "cron 0 * * * *".into(), entry: "perceive".into() },
            DutyInfo { id: "ghost-duty".into(), trigger: "cron 0 0 30 2 *".into(), entry: "perceive".into() },
        ]);
        // x secret dead; cove ok.
        reg.record_secret_ok("cove_read", now - 200);
        reg.record_secret_err("x", "refresh rejected (invalid_grant) — RE-AUTH NEEDED", now - 50);
        // twitter-mentions failing (dead secret drop); heartbeat healthy; ghost-duty never fires.
        reg.record_stimulus_fired("twitter-mentions", now - 60);
        reg.record_stimulus_err("twitter-mentions", "mcp twitter-read: X_BEARER_TOKEN not set");
        reg.record_stimulus_fired("heartbeat", now - 120);
        reg.record_stimulus_ok("heartbeat", now - 119);

        let out = reg.snapshot().render(now);
        // Secrets: x flagged (because one is bad, secrets go to detail mode), cove not flagged.
        assert!(out.contains("x: FAILING"), "{out}");
        assert!(out.contains("RE-AUTH NEEDED"), "{out}");
        // The whole roster shows, including the duty that never fired.
        assert!(out.contains("twitter-mentions [cron */2 * * * * → perceive]: FAILING"), "{out}");
        assert!(out.contains("heartbeat [cron 0 * * * * → perceive]: ok"), "{out}");
        assert!(out.contains("ghost-duty") && out.contains("never fired since boot"), "{out}");
        assert!(out.contains("Uptime 1h0m"), "{out}");
    }

    #[test]
    fn render_stays_terse_when_all_green() {
        let now = 500;
        let reg = HealthRegistry::new(now - 120);
        reg.set_duties(vec![DutyInfo { id: "heartbeat".into(), trigger: "cron 0 * * * *".into(), entry: "perceive".into() }]);
        reg.record_secret_ok("x", now - 10);
        reg.record_secret_ok("cove_read", now - 10);
        reg.record_stimulus_fired("heartbeat", now - 30);
        reg.record_stimulus_ok("heartbeat", now - 29);
        let out = reg.snapshot().render(now);
        assert!(out.contains("Secrets: all ok — cove_read, x"), "{out}"); // BTreeMap ⇒ sorted
        assert!(!out.contains("FAILING"), "{out}");
    }
}
