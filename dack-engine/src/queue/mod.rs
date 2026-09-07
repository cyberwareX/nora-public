//! The stimulus queue — **single-flight** (concurrency = 1; the duck is one mind,
//! Harness writer-of-record; the agent never sees it
//! v1 backing store is embedded SQLite on the VPS (ephemeral — losing it
//! loses only the queue, not the soul).
//!
//! A hand-rolled priority queue over SQLite is fine at single-agent scale — single-
//! flight makes it trivial. SCAFFOLD: an in-memory [`InMemoryQueue`] makes
//! the loop runnable now; the SQLite impl lands in a later step.
use async_trait::async_trait;

use crate::error::Result;
use crate::model::stimulus::{Stimulus, StimulusId, StimulusStatus, StimulusType};

#[async_trait]
pub trait Queue: Send + Sync {
    async fn enqueue(&self, stimulus: Stimulus) -> Result<()>;

    /// Pop the highest-priority `pending` row and mark it `dispatched`. Single-flight:
    /// returns at most one, and the harness processes it to completion before the next.
    async fn next(&self) -> Result<Option<Stimulus>>;

    async fn update_status(&self, id: &StimulusId, status: StimulusStatus) -> Result<()>;

    /// Re-pend a row after a zero-completion model timeout: set its new `attempts` count and gate it
    /// behind `pop_after` (a backoff), so the dispatch loop re-attempts a hung-but-side-effect-free
    /// cycle (bounded by the cap) instead of failing it terminally. Updates the same row in place.
    async fn reschedule(&self, id: &StimulusId, attempts: u32, pop_after: i64) -> Result<()>;

    /// Boot reconciliation: requeue rows stuck in `dispatched` — a crash mid-run
    /// orphans the in-flight row (`next` flipped it `dispatched`, nothing advanced it). v1
    /// effects are reversible (post/memory, never Settle), so **requeue is safe**: the duck
    /// reconsiders the stimulus rather than silently dropping it. Single-flight means there is
    /// at most one such row. Returns how many were reclaimed (logged at boot).
    async fn reclaim_orphans(&self) -> Result<usize>;

    /// Replace a row's payload — used by the bus to fold a `batch`-coalesced candidate
    /// into the pending accumulator row. Policy stays in the bus; the queue is
    /// a dumb store.
    async fn set_payload(&self, id: &StimulusId, payload: serde_json::Value) -> Result<()>;

    /// Rows eligible to coalesce with an incoming candidate (same type + dedup_key,
    /// still `pending`) — "coalesce".
    async fn find_coalescable(
        &self,
        type_: &StimulusType,
        dedup_key: &str,
    ) -> Result<Vec<Stimulus>>;

    /// Queue depth, for `dack status`.
    async fn depth(&self) -> Result<usize>;

    /// Read a cross-poll dedup **cursor** (watermark) by key — `None` until first set. The
    /// harness injects it into a polling sensor's env so it fetches only newer items.
    async fn get_cursor(&self, key: &str) -> Result<Option<String>>;

    /// Persist a cursor watermark (insert-or-replace). Called after a poll with the max seen
    /// value; single-flight makes the read-modify-write race-free.
    async fn set_cursor(&self, key: &str, value: &str) -> Result<()>;

    /// **Load-shedding**: if more than `max_depth` rows are PENDING, delete the OLDEST
    /// `Low`-priority pending rows (the stalest, least-urgent work) until at/under the cap or none
    /// remain — Normal+ is the protected floor and is NEVER shed. Returns the shed ids (the harness
    /// logs them; no silent truncation). A queue still over cap with no `Low` to shed is genuine
    /// backpressure, not a drop. Default: a no-op store with no notion of bounding.
    async fn shed(&self, _max_depth: usize) -> Result<Vec<StimulusId>> {
        Ok(Vec::new())
    }
}

mod memory_queue;
mod sqlite;
pub use memory_queue::InMemoryQueue;
pub use sqlite::SqliteQueue;
