//! In-memory single-flight queue — scaffold backing for [`Queue`] so the harness loop
//! runs before the SQLite impl exists (swaps in `rusqlite`). Concurrency = 1
//! is enforced by the harness loop, not here; this just orders by priority.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

use super::Queue;
use crate::error::Result;
use crate::model::stimulus::{Priority, Stimulus, StimulusId, StimulusStatus, StimulusType};

#[derive(Default)]
pub struct InMemoryQueue {
    rows: Mutex<Vec<Stimulus>>,
    cursors: Mutex<HashMap<String, String>>,
}

impl InMemoryQueue {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Queue for InMemoryQueue {
    async fn enqueue(&self, stimulus: Stimulus) -> Result<()> {
        self.rows.lock().unwrap().push(stimulus);
        Ok(())
    }

    async fn next(&self) -> Result<Option<Stimulus>> {
        let now = chrono::Utc::now().timestamp();
        let mut rows = self.rows.lock().unwrap();
        // Highest priority = lowest numeric, then oldest received_at. A still-debounced row
        // (`pop_after > now`) is skipped so its coalesce window keeps accumulating.
        let idx = rows
            .iter()
            .enumerate()
            .filter(|(_, s)| s.status == StimulusStatus::Pending)
            .filter(|(_, s)| s.pop_after.map_or(true, |t| t <= now))
            .min_by_key(|(_, s)| (s.priority.numeric(), s.received_at))
            .map(|(i, _)| i);
        match idx {
            Some(i) => {
                rows[i].status = StimulusStatus::Dispatched;
                Ok(Some(rows[i].clone()))
            }
            None => Ok(None),
        }
    }

    async fn update_status(&self, id: &StimulusId, status: StimulusStatus) -> Result<()> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(s) = rows.iter_mut().find(|s| &s.id == id) {
            s.status = status;
        }
        Ok(())
    }

    async fn reschedule(&self, id: &StimulusId, attempts: u32, pop_after: i64) -> Result<()> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(s) = rows.iter_mut().find(|s| &s.id == id) {
            s.status = StimulusStatus::Pending;
            s.attempts = attempts;
            s.pop_after = Some(pop_after);
        }
        Ok(())
    }

    async fn reclaim_orphans(&self) -> Result<usize> {
        let mut rows = self.rows.lock().unwrap();
        let mut n = 0;
        for s in rows.iter_mut().filter(|s| s.status == StimulusStatus::Dispatched) {
            s.status = StimulusStatus::Pending;
            n += 1;
        }
        Ok(n)
    }

    async fn set_payload(&self, id: &StimulusId, payload: serde_json::Value) -> Result<()> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(s) = rows.iter_mut().find(|s| &s.id == id) {
            s.payload = payload;
        }
        Ok(())
    }

    async fn find_coalescable(
        &self,
        type_: &StimulusType,
        dedup_key: &str,
    ) -> Result<Vec<Stimulus>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .iter()
            .filter(|s| {
                s.status == StimulusStatus::Pending
                    && &s.type_ == type_
                    && s.dedup_key.as_deref() == Some(dedup_key)
            })
            .cloned()
            .collect())
    }

    async fn depth(&self) -> Result<usize> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.status == StimulusStatus::Pending)
            .count())
    }

    async fn get_cursor(&self, key: &str) -> Result<Option<String>> {
        Ok(self.cursors.lock().unwrap().get(key).cloned())
    }

    async fn set_cursor(&self, key: &str, value: &str) -> Result<()> {
        self.cursors.lock().unwrap().insert(key.to_string(), value.to_string());
        Ok(())
    }

    async fn shed(&self, max_depth: usize) -> Result<Vec<StimulusId>> {
        let mut rows = self.rows.lock().unwrap();
        let pending = rows.iter().filter(|s| s.status == StimulusStatus::Pending).count();
        if pending <= max_depth {
            return Ok(Vec::new());
        }
        let excess = pending - max_depth;
        let low_rank = Priority::Low.numeric();
        // Indices of the OLDEST low-priority pending rows (stalest, least-urgent), `excess` of them.
        let mut cand: Vec<(usize, i64)> = rows
            .iter()
            .enumerate()
            .filter(|(_, s)| s.status == StimulusStatus::Pending && s.priority.numeric() >= low_rank)
            .map(|(i, s)| (i, s.received_at))
            .collect();
        cand.sort_by_key(|(_, r)| *r);
        let victims: std::collections::HashSet<usize> =
            cand.into_iter().take(excess).map(|(i, _)| i).collect();
        let mut shed = Vec::new();
        let mut idx = 0usize;
        rows.retain(|s| {
            let keep = !victims.contains(&idx);
            if !keep {
                shed.push(s.id.clone());
            }
            idx += 1;
            keep
        });
        Ok(shed)
    }
}
