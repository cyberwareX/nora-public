//! Cron source — a tiny timer wheel over the registered cron
//! duties. Each tick emits a [`FiredTrigger`] (empty payload — a cron fire carries no
//! world data; the duty's sensor, if any, does the perceiving). **Zero reasoning here**
//! the wheel only decides *when*, never *what*.
//!
//! Hot-reload: [`reschedule`](CronWheel::reschedule) swaps the schedule set and wakes the
//! loop, so a Reflect-authored `stimuli/` change re-times the duck's alarm clock without a
//! restart.

use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use cron::Schedule;
use tokio::sync::{mpsc, Notify};

use super::{CronScheduler, FiredTrigger};
use crate::error::Result;

/// Standard 5-field cron (`min hour dom month dow`) → the `cron` crate's 6-field form by
/// prepending a `0` seconds column. 6/7-field expressions pass through unchanged. This is
/// why the `stimuli/` examples can use familiar `"0 */4 * * *"`.
fn normalize_cron(expr: &str) -> String {
    let expr = expr.trim();
    if expr.split_whitespace().count() == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    }
}

/// Parse a (possibly 5-field) cron expression.
pub fn parse_cron(expr: &str) -> Result<Schedule> {
    Schedule::from_str(&normalize_cron(expr))
        .map_err(|e| crate::error::DackError::Stimulus(format!("bad cron `{expr}`: {e}")))
}

/// The next fire strictly after `after`, or `None` if the schedule never fires again.
/// Pure + deterministic — the unit-testable core of the wheel.
pub fn next_fire(schedule: &Schedule, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    schedule.after(&after).next()
}

pub struct CronWheel {
    /// `(def_id, schedule)` — replaced wholesale on reschedule.
    schedules: Mutex<Vec<(String, Schedule)>>,
    /// Wakes the run loop when the schedule set changes.
    notify: Notify,
    tx: mpsc::Sender<FiredTrigger>,
}

impl CronWheel {
    /// Build the wheel over a caller-owned `FiredTrigger` channel (shared with the webhook
    /// listener so the harness drains a single unified stream). The loop is started
    /// separately via [`run`](Self::run) so the caller owns its task handle.
    pub fn new(tx: mpsc::Sender<FiredTrigger>) -> Arc<Self> {
        Arc::new(Self {
            schedules: Mutex::new(Vec::new()),
            notify: Notify::new(),
            tx,
        })
    }

    /// The wheel's event loop: sleep until the soonest next fire (or a reschedule), emit a
    /// `FiredTrigger` for every duty due, repeat. A bad cron expression is skipped at
    /// `reschedule` time, so the loop only ever holds valid schedules.
    pub async fn run(self: Arc<Self>) {
        loop {
            let snapshot = self.schedules.lock().unwrap().clone();
            let now = Utc::now();

            let soonest = snapshot
                .iter()
                .filter_map(|(_, s)| next_fire(s, now))
                .min();

            let sleep = match soonest {
                // `to_std` fails only for a negative/zero span → fire immediately.
                Some(t) => (t - now).to_std().unwrap_or(Duration::from_millis(0)),
                // Nothing scheduled: park until a reschedule wakes us.
                None => Duration::from_secs(3600),
            };

            tokio::select! {
                _ = tokio::time::sleep(sleep) => {
                    let fire_now = Utc::now();
                    for (id, sched) in &snapshot {
                        if next_fire(sched, now).is_some_and(|t| t <= fire_now) {
                            // Receiver gone → harness is shutting down; stop the loop.
                            if self.tx.send(FiredTrigger { def_id: id.clone(), payload: vec![] }).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                _ = self.notify.notified() => { /* schedules changed — recompute */ }
            }
        }
    }
}

#[async_trait::async_trait]
impl CronScheduler for CronWheel {
    async fn reschedule(&self, crons: &[(String, String)]) -> Result<()> {
        let mut parsed = Vec::with_capacity(crons.len());
        for (def_id, expr) in crons {
            match parse_cron(expr) {
                Ok(s) => parsed.push((def_id.clone(), s)),
                // One bad cron must not silence every other timer (logging-not-rollback).
                Err(e) => tracing::warn!("cron: skipping duty `{def_id}`: {e}"),
            }
        }
        *self.schedules.lock().unwrap() = parsed;
        self.notify.notify_one();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn normalizes_five_field_cron() {
        assert_eq!(normalize_cron("0 * * * *"), "0 0 * * * *");
        assert_eq!(normalize_cron("* * * * * *"), "* * * * * *"); // 6-field untouched
    }

    #[test]
    fn next_fire_is_deterministic() {
        // Hourly at minute 0. From 09:30:00Z the next fire is 10:00:00Z.
        let sched = parse_cron("0 * * * *").unwrap();
        let after = Utc.with_ymd_and_hms(2026, 6, 5, 9, 30, 0).unwrap();
        let next = next_fire(&sched, after).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 6, 5, 10, 0, 0).unwrap());
    }

    #[test]
    fn rejects_garbage_cron() {
        assert!(parse_cron("not a cron").is_err());
    }

    #[tokio::test]
    async fn fires_a_trigger_on_a_fast_schedule() {
        let (tx, mut rx) = mpsc::channel(64);
        let wheel = CronWheel::new(tx);
        wheel
            .reschedule(&[("tick".to_string(), "* * * * * *".to_string())]) // every second
            .await
            .unwrap();
        tokio::spawn(wheel.clone().run());

        // Within ~2 ticks we must receive a fire for the registered duty.
        let fired = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("a cron tick fired within 3s")
            .expect("channel open");
        assert_eq!(fired.def_id, "tick");
        assert!(fired.payload.is_empty());
    }
}
