//! The Stimulus Bus (layer 2) — **dumb, central, no LLM.**
//! Intelligence is quarantined in the consciousness layer; a reasoning collector here
//! would be an unguarded injection surface upstream of the firebreak.
//!
//! Pipeline: normalize → classify → prioritize → coalesce → **serialize (single-flight)**
//! → enqueue. The agent influences priority and authors its own duty-stimuli (in
//! Reflect); it never edits the routing table or assigns tiers.

use std::sync::Arc;

use crate::config::{CoalesceMode, DackConfig};
use crate::error::Result;
use crate::model::stimulus::{
    Priority, Stimulus, StimulusId, StimulusStatus, StimulusType, TrustTier,
};
use crate::queue::Queue;
use crate::sensor::SensorCandidate;
use crate::stimuli::StimulusDef;

pub struct Bus {
    config: Arc<DackConfig>,
    queue: Arc<dyn Queue>,
}

impl Bus {
    pub fn new(config: Arc<DackConfig>, queue: Arc<dyn Queue>) -> Self {
        Self { config, queue }
    }

    /// Normalize sensor candidates from one duty firing into `Stimulus` rows and enqueue
    /// them, applying the duty's coalescing policy. `received_at` is
    /// supplied by the caller (the harness stamps wall-clock time) so the bus stays
    /// deterministic and testable. Returns the ids of *newly enqueued* rows (a candidate
    /// folded into an existing batch row mints no new wake).
    /// `payload_seed` is the cycle's PAYLOAD trust seed — derived by the [`Ingestor`] from
    /// the source (signed-script hash / webhook tier / pure-cron `self`), met with the sensor
    /// secrets. Each candidate's tier is clamped DOWN to it (a sensor may only lower trust).
    pub async fn ingest(
        &self,
        def: &StimulusDef,
        candidates: Vec<SensorCandidate>,
        received_at: i64,
        payload_seed: TrustTier,
    ) -> Result<Vec<StimulusId>> {
        let fm = &def.frontmatter;
        let mut enqueued = Vec::new();

        for (i, c) in candidates.into_iter().enumerate() {
            let SensorCandidate {
                type_,
                payload,
                dedup_key,
                payload_tier: cand_tier,
            } = c;
            // The source assigns the tier; a sensor may only *LOWER* it. A candidate claiming a tier
            // above the source-derived `payload_seed` is clamped down (the taint `meet`) — closing a
            // trust-tier escalation primitive (arbitrary Reflect-authored sensor code).
            let lattice = self.config.lattice();
            let payload_tier = match cand_tier {
                Some(c) => lattice.meet(&c, &payload_seed),
                None => payload_seed.clone(),
            };
            // Priority is the duty's own frontmatter hint (the routing table is gone).
            let priority = fm.priority.unwrap_or(Priority::Low);
            // Entry **state-prompt** (): the duty's own frontmatter `entry:` (a state-prompt
            // id like `twitter/perceive_mention`). The path the chain then walks is soul-owned; the
            // operator route only supplies the *ceiling* (read at dispatch), not the entry.
            let entry = fm.entry.clone();

            // Coalescing. `None` → always a fresh wake; `Latest` → supersede
            // prior pending rows for this key and enqueue the newest; `Batch` → fold the
            // payload into the oldest pending accumulator row within the window (no new wake).
            let mut folded = false;
            if let (Some(policy), Some(key)) = (&fm.coalesce, dedup_key.as_deref()) {
                match policy.mode {
                    CoalesceMode::None => {}
                    CoalesceMode::Latest => {
                        for s in self
                            .within_window(&type_, key, received_at, policy.window_sec)
                            .await?
                        {
                            self.queue
                                .update_status(&s.id, StimulusStatus::Coalesced)
                                .await?;
                        }
                    }
                    CoalesceMode::Batch => {
                        // Fixed mode bounds coalescable rows by `window_sec`; adaptive's window is
                        // variable (encoded per-row in `pop_after`), so scan unbounded and fold only
                        // into an accumulator whose window is still OPEN (`received_at < pop_after`) —
                        // never into one already firing. The fixed path is unchanged (`find` on a
                        // `true` predicate = the old `.next()`).
                        let bound = if policy.adaptive.is_some() { None } else { policy.window_sec };
                        let acc = self
                            .within_window(&type_, key, received_at, bound)
                            .await?
                            .into_iter()
                            .find(|s| {
                                policy.adaptive.is_none()
                                    || s.pop_after.map_or(false, |p| received_at < p)
                            });
                        if let Some(acc) = acc {
                            // Fold into the accumulator; keep its received_at (FIFO fairness)
                            // so a hot thread can't starve older pending stimuli.
                            let merged = merge_batch(acc.payload, &payload);
                            self.queue.set_payload(&acc.id, merged).await?;
                            folded = true;
                        }
                    }
                }
            }

            if folded {
                continue;
            }

            // Debounce gate: a `batch` duty with a positive `window_sec` holds this NEW accumulator
            // row unpoppable until `received_at + window_sec`, so the chat's later messages fold into
            // it (above) before it fires as one wake. Folds keep the accumulator's `received_at`, so
            // the window is measured from the FIRST message (a fixed cadence, not a sliding one). A
            // `window_sec` of 0/absent (or `latest`/`none`) ⇒ `None` = immediately poppable.
            // Both the coalesce debounce AND the dispatch window express themselves as a
            // `pop_after` (earliest-poppable) second; the effective gate is the LATER of the two —
            // a windowed group message still folds for its debounce, then waits for the window.
            let coalesce_pop_after = match &fm.coalesce {
                Some(p) if matches!(p.mode, CoalesceMode::Batch) => match &p.adaptive {
                    // Adaptive: size this NEW accumulator's window from the chat's daily credit budget,
                    // then SPEND one credit (this accumulator becomes one wake). A noisier chat burns
                    // its budget → the window grows → it slows itself down. Per-chat (dedup_key), reset
                    // daily (date in the cursor key). Spending here (at creation, not dispatch) keeps the
                    // accounting self-contained and lets the next accumulator see the new count at once.
                    Some(ad) => match dedup_key.as_deref() {
                        Some(chat) => {
                            let key = adaptive_credit_key(&fm.id, chat, received_at);
                            let spent = self
                                .queue
                                .get_cursor(&key)
                                .await?
                                .and_then(|v| v.parse::<u64>().ok())
                                .unwrap_or(0);
                            let window = ad.window_sec(spent);
                            self.queue.set_cursor(&key, &(spent + 1).to_string()).await?;
                            // Observable: the window stepping up as a chat spends its daily budget.
                            tracing::debug!(
                                "coalesce[adaptive] {}:{} spent={}/{} → window={window}s",
                                fm.id,
                                chat,
                                spent + 1,
                                ad.daily_credits
                            );
                            Some(received_at + window as i64)
                        }
                        // No per-chat key to budget against — fall back to the initial window.
                        None => Some(received_at + ad.initial_window_sec as i64),
                    },
                    None => p.window_sec.filter(|w| *w > 0).map(|w| received_at + w as i64),
                },
                _ => None,
            };
            let window_pop_after = fm
                .dispatch_window
                .as_deref()
                .and_then(crate::stimuli::DispatchWindow::parse)
                .map(|w| w.next_open(received_at))
                .filter(|t| *t > received_at);
            let pop_after = match (coalesce_pop_after, window_pop_after) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };

            let id = StimulusId(format!("{}-{}-{}", fm.id, received_at, i));
            self.queue
                .enqueue(Stimulus {
                    id: id.clone(),
                    source: fm.id.clone(),
                    type_,
                    directive_tier: fm.directive_tier.clone(),
                    payload_tier,
                    payload,
                    provenance: None,
                    received_at,
                    dedup_key,
                    pop_after,
                    priority,
                    status: StimulusStatus::Pending,
                    attempts: 0,
                    directive_body: def.directive_body.clone(),
                    entry,
                })
                .await?;
            enqueued.push(id);
        }
        Ok(enqueued)
    }

    /// Pending rows that can coalesce with an incoming candidate: same `(type, dedup_key)`,
    /// still within `window_sec` of `now` (unbounded when `None`), oldest first (so the
    /// first is the batch accumulator).
    async fn within_window(
        &self,
        type_: &StimulusType,
        key: &str,
        now: i64,
        window_sec: Option<u64>,
    ) -> Result<Vec<Stimulus>> {
        let mut rows = self.queue.find_coalescable(type_, key).await?;
        if let Some(w) = window_sec {
            let w = w as i64;
            rows.retain(|s| now - s.received_at <= w);
        }
        rows.sort_by_key(|s| s.received_at);
        Ok(rows)
    }

}

/// Per-chat, per-UTC-day cursor key for the adaptive coalesce credit budget. `received_at / 86_400` is
/// the epoch-day, so a new day naturally yields a fresh key (= a fresh budget) — no explicit reset.
fn adaptive_credit_key(source: &str, dedup_key: &str, received_at: i64) -> String {
    format!("credits:{source}:{dedup_key}:{}", received_at / 86_400)
}

/// Fold a new payload into a batch accumulator. Shape: `{<latest msg's fields…>, "_coalesced": true,
/// "items":[…]}`. The first fold wraps the existing single payload; subsequent folds push onto `items`.
/// Context assembly renders a coalesced row from `items` ("N things since last wake").
///
/// **The latest message's top-level fields are hoisted onto the accumulator** (the new payload's
/// scalars overlay the row, while `items` keeps the full history). Same-chat folds share their
/// thread-invariant fields (`chat_id`), and per-message fields (`message_id`, `from_username`) track
/// the latest — so a payload-field consumer that runs on the *row* (the `scope_env` reply
/// destination-lock → the chat + the newest message; the baton's `source_*` refs) still resolves,
/// pointing at the most recent message. Without this, batching would bury `chat_id` inside `items`
/// and a coalesced wake could not be replied to.
fn merge_batch(existing: serde_json::Value, new: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    // Start from the latest message's fields (so top-level tracks the newest), then attach the
    // accumulated `items` + the `_coalesced` marker.
    let mut out = match new {
        Value::Object(m) => m.clone(),
        other => {
            let mut m = serde_json::Map::new();
            m.insert("value".into(), other.clone());
            m
        }
    };
    let items = match existing {
        Value::Object(mut m) if m.get("_coalesced") == Some(&Value::Bool(true)) => {
            match m.remove("items") {
                Some(Value::Array(mut items)) => {
                    items.push(new.clone());
                    items
                }
                _ => vec![new.clone()],
            }
        }
        other => vec![other, new.clone()],
    };
    out.insert("_coalesced".into(), Value::Bool(true));
    out.insert("items".into(), Value::Array(items));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::InMemoryQueue;
    use crate::sensor::SensorCandidate;
    use serde_json::json;

    fn bus() -> Bus {
        let config = Arc::new(DackConfig::from_yaml("operator_did: \"did:key:zOp\"").unwrap());
        Bus::new(config, Arc::new(InMemoryQueue::new()))
    }

    /// A duty whose frontmatter carries the given coalesce policy line (or none).
    fn duty(coalesce_line: &str) -> StimulusDef {
        let text = format!(
            "---\nid: mentions\ntrigger: {{ type: cron, schedule: \"0 * * * *\" }}\n\
             directive_tier: self\nemits:\n  type: mention\n  default_payload_tier: public\n\
             {coalesce_line}entry: perceive\n---\nReply-guy duty.\n"
        );
        StimulusDef::parse(&text, "stimuli/mentions/STIMULUS.md").unwrap()
    }

    fn cand(text: &str, key: &str) -> SensorCandidate {
        SensorCandidate {
            type_: StimulusType::from("mention"),
            payload: json!({ "text": text }),
            dedup_key: Some(key.into()),
            payload_tier: None,
        }
    }

    /// A chat-shaped candidate (telegram): a per-chat thread key + chat_id/message_id fields.
    fn chat_cand(chat: i64, msg: i64, text: &str) -> SensorCandidate {
        SensorCandidate {
            type_: StimulusType::from("telegram_message"),
            payload: json!({ "chat_id": chat, "message_id": msg, "text": text }),
            dedup_key: Some(chat.to_string()), // the THREAD key (per-chat), not per-message
            payload_tier: None,
        }
    }

    /// The telegram group case: a chat's messages fold into ONE debounced wake, and the coalesced
    /// row keeps the LATEST message's `chat_id`/`message_id` at top-level (so the reply
    /// destination-lock + baton still resolve) while `items` holds the whole batch.
    /// dispatch window: a duty with `dispatch_window: "02:00-05:00"` holds a stimulus that
    /// arrives outside the window (via `pop_after`) until it next opens; one arriving inside is
    /// immediately eligible.
    #[tokio::test]
    async fn dispatch_window_defers_a_stimulus_until_the_window_opens() {
        let def = StimulusDef::parse(
            "---\nid: pubgroup\ntrigger: { type: webhook, path: /telegram/pub }\n\
             directive_tier: self\nemits:\n  type: telegram_message\n  default_payload_tier: public\n\
             dispatch_window: \"02:00-05:00\"\nentry: telegram/perceive\n---\nNoisy public group.\n",
            "stimuli/pubgroup/STIMULUS.md",
        )
        .unwrap();

        // Arrives at 00:00 UTC (received_at=0) — OUTSIDE the window → held until 02:00 (7200s).
        let b = bus();
        b.ingest(&def, vec![chat_cand(9, 1, "gm at midnight")], 0, TrustTier::public()).await.unwrap();
        let row = b.queue.next().await.unwrap().unwrap();
        assert_eq!(row.pop_after, Some(7_200), "held until the window opens at 02:00 UTC");

        // Arrives at 03:00 UTC (received_at=10800) — INSIDE the window → no gate.
        let b2 = bus();
        b2.ingest(&def, vec![chat_cand(9, 2, "gm at 3am")], 10_800, TrustTier::public()).await.unwrap();
        let row2 = b2.queue.next().await.unwrap().unwrap();
        assert_eq!(row2.pop_after, None, "inside the window → immediately poppable");
    }

    #[tokio::test]
    async fn batch_hoists_latest_fields_and_sets_pop_after() {
        let b = bus();
        let def = StimulusDef::parse(
            "---\nid: tg\ntrigger: { type: webhook, path: /telegram/pub }\n\
             directive_tier: self\nemits:\n  type: telegram_message\n\
             coalesce: { mode: batch, window_sec: 90 }\nentry: telegram/perceive\n---\nGroup.\n",
            "stimuli/tg/STIMULUS.md",
        )
        .unwrap();
        // Three messages in the same chat (thread key = chat_id) at received_at=1000.
        b.ingest(
            &def,
            vec![chat_cand(7, 1, "gm"), chat_cand(7, 2, "wen"), chat_cand(7, 3, "ser")],
            1000,
            TrustTier::public(),
        )
        .await
        .unwrap();
        assert_eq!(b.queue.depth().await.unwrap(), 1, "the chat folds into one wake");
        // The debounced row is held until its window passes (pop_after = 1000 + 90, far in the past
        // vs the wall clock here, so it IS poppable in this test).
        let row = b.queue.next().await.unwrap().unwrap();
        assert_eq!(row.pop_after, Some(1090), "debounce gate = received_at + window_sec");
        assert_eq!(row.payload.get("chat_id").and_then(|v| v.as_i64()), Some(7), "chat hoisted");
        assert_eq!(
            row.payload.get("message_id").and_then(|v| v.as_i64()),
            Some(3),
            "the LATEST message_id is hoisted (reply targets the newest)"
        );
        let items = row.payload.get("items").and_then(|v| v.as_array()).unwrap();
        assert_eq!(items.len(), 3, "every message kept in items for context");
        assert!(row.payload.get("_coalesced").unwrap().as_bool().unwrap());
    }

    /// An adaptive duty. As a chat spends its daily credits, the new-accumulator window grows (×10 each
    /// time half the remaining budget is burned), and exactly one credit is spent per wake.
    fn adaptive_duty(credits: u64, initial: u64) -> StimulusDef {
        let text = format!(
            "---\nid: tg\ntrigger: {{ type: webhook, path: /telegram/trusted }}\n\
             directive_tier: self\nemits:\n  type: telegram_message\n\
             coalesce: {{ mode: batch, adaptive: {{ initial_window_sec: {initial}, daily_credits: {credits}, max_window_sec: 1800 }} }}\n\
             entry: telegram/perceive\n---\nGroup.\n"
        );
        StimulusDef::parse(&text, "stimuli/tg/STIMULUS.md").unwrap()
    }

    #[tokio::test]
    async fn adaptive_window_grows_as_a_chat_spends_its_credits() {
        let b = bus();
        let def = adaptive_duty(2, 10); // tiny budget so one wake crosses the half-spent threshold
        let key = format!("credits:tg:7:{}", 1000 / 86_400);

        // First burst, chat 7: spent 0, remaining 2 → step 0 → initial 10s; one credit spent.
        b.ingest(&def, vec![chat_cand(7, 1, "gm")], 1000, TrustTier::public()).await.unwrap();
        let r1 = b.queue.next().await.unwrap().unwrap();
        assert_eq!(r1.pop_after, Some(1010), "full budget → initial window");
        assert_eq!(b.queue.get_cursor(&key).await.unwrap().as_deref(), Some("1"), "one wake = one credit");

        // Second burst (r1 already popped → new accumulator): spent 1, remaining 1 → step 1 → ×10.
        b.ingest(&def, vec![chat_cand(7, 2, "wen")], 2000, TrustTier::public()).await.unwrap();
        let r2 = b.queue.next().await.unwrap().unwrap();
        assert_eq!(r2.pop_after, Some(2100), "half the budget spent → window ×10");
        assert_eq!(b.queue.get_cursor(&key).await.unwrap().as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn adaptive_folds_spend_one_credit_per_batch_not_per_message() {
        let b = bus();
        let def = adaptive_duty(100, 10);
        // Three messages in one chat fold into ONE accumulator → exactly one credit.
        b.ingest(
            &def,
            vec![chat_cand(7, 1, "a"), chat_cand(7, 2, "b"), chat_cand(7, 3, "c")],
            1000,
            TrustTier::public(),
        )
        .await
        .unwrap();
        assert_eq!(b.queue.depth().await.unwrap(), 1, "the chat folds into one wake");
        let key = format!("credits:tg:7:{}", 1000 / 86_400);
        assert_eq!(
            b.queue.get_cursor(&key).await.unwrap().as_deref(),
            Some("1"),
            "a batch costs one credit, not one per message"
        );
    }

    #[tokio::test]
    async fn none_keeps_every_candidate() {
        let b = bus();
        let def = duty("coalesce: { mode: none }\n");
        let ids = b
            .ingest(&def, vec![cand("a", "t1"), cand("b", "t1"), cand("c", "t1")], 1000, TrustTier::self_())
            .await
            .unwrap();
        assert_eq!(ids.len(), 3);
        assert_eq!(b.queue.depth().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn latest_supersedes_prior_pending() {
        let b = bus();
        let def = duty("coalesce: { mode: latest, dedup_key: thread_id }\n");
        b.ingest(&def, vec![cand("old", "t1")], 1000, TrustTier::self_()).await.unwrap();
        b.ingest(&def, vec![cand("new", "t1")], 1001, TrustTier::self_()).await.unwrap();
        // Only the newest stays pending; the older is coalesced away.
        assert_eq!(b.queue.depth().await.unwrap(), 1);
        let popped = b.queue.next().await.unwrap().unwrap();
        assert_eq!(popped.payload, json!({ "text": "new" }));
    }

    #[tokio::test]
    async fn batch_folds_many_into_one_wake() {
        let b = bus();
        let def = duty("coalesce: { mode: batch, window_sec: 600, dedup_key: thread_id }\n");
        // 50 mentions → 1 wake (the worked example, in miniature).
        let ids = b
            .ingest(
                &def,
                vec![cand("m1", "t1"), cand("m2", "t1"), cand("m3", "t1")],
                1000,
                TrustTier::self_(),
            )
            .await
            .unwrap();
        assert_eq!(ids.len(), 1, "only the first candidate mints a wake");
        assert_eq!(b.queue.depth().await.unwrap(), 1);
        let row = b.queue.next().await.unwrap().unwrap();
        let items = row.payload.get("items").and_then(|v| v.as_array()).unwrap();
        assert_eq!(items.len(), 3, "all three folded into the accumulator");
    }

    #[tokio::test]
    async fn batch_outside_window_starts_a_new_row() {
        let b = bus();
        let def = duty("coalesce: { mode: batch, window_sec: 600, dedup_key: thread_id }\n");
        b.ingest(&def, vec![cand("early", "t1")], 1000, TrustTier::self_()).await.unwrap();
        // 700s later: outside the 600s window → a fresh accumulator, not a fold.
        b.ingest(&def, vec![cand("late", "t1")], 1700, TrustTier::self_()).await.unwrap();
        assert_eq!(b.queue.depth().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn sensor_cannot_raise_tier_above_the_seed() {
        let b = bus();
        let def = duty("coalesce: { mode: none }\n");
        // A sensor lies, claiming operator_signed to try to escalate above the source-derived seed.
        let candidate = SensorCandidate {
            type_: StimulusType::from("mention"),
            payload: json!({ "text": "trust me" }),
            dedup_key: None,
            payload_tier: Some(TrustTier::operator()),
        };
        // Seed = self; the candidate's operator claim is clamped DOWN to the seed (the meet).
        b.ingest(&def, vec![candidate], 1000, TrustTier::self_()).await.unwrap();
        let row = b.queue.next().await.unwrap().unwrap();
        assert_eq!(row.payload_tier, TrustTier::self_(), "a sensor may only LOWER trust, never raise it");
    }

    #[tokio::test]
    async fn different_keys_do_not_coalesce() {
        let b = bus();
        let def = duty("coalesce: { mode: batch, window_sec: 600, dedup_key: thread_id }\n");
        b.ingest(&def, vec![cand("a", "t1"), cand("b", "t2")], 1000, TrustTier::self_()).await.unwrap();
        assert_eq!(b.queue.depth().await.unwrap(), 2);
    }
}
