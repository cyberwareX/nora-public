//! The agent's I/O contract. Conceptually the agent has **no free-form
//! output channel** — every external effect is a skill (Twitter via skill, DAC via
//! skill+MCP, commits via gl-MCP). The agent's *return value* to the harness is this
//! small structured object: reasoning + whether a state transition is requested.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::stimulus::Priority;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Reply,
    Post,
    Research,
    /// Hand a real, longer job to a keyless sandboxed worker. Declared in Perceive to
    /// signal the intent; the actual [`SpawnRequest`] is honored only once the duck reaches Express.
    Delegate,
    Ignore,
    Noop,
    /// Catch-all for any intent label the model emits that we don't model explicitly. `intent` is
    /// **descriptive only** — control flow is driven by `transition.to_prompt`/`spawn`, never by
    /// `intent` — so an unrecognized label must degrade here, NEVER hard-error the cycle.
    #[serde(other)]
    Other,
}

/// The digested proposal Perceive hands forward — becomes the [`super::baton::Baton`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub intent: Intent,
    /// Digested intent — NOT raw stimulus text. `default` so a gist-less proposal degrades.
    #[serde(default)]
    pub gist: String,
    /// The duck's own soft reference annotations. The harness-critical refs (`source_tweet_id`,
    /// `source_author`) are injected DETERMINISTICALLY into the Baton from the stimulus payload, not
    /// from here — so this is non-load-bearing and accepts whatever shape the model emits (object,
    /// array, scalar, or null), normalized to a string map. Never hard-errors the cycle.
    #[serde(default, deserialize_with = "de_refs")]
    pub refs: BTreeMap<String, String>,
}

/// Lenient `refs` deserializer — a weaker model emits this field inconsistently (a `["a","b"]`
/// array, a `{"k":"v"}` object, a bare string, or `null`). All normalize to `BTreeMap<String,
/// String>`; arrays/scalars get index keys. Non-string values are stringified. This keeps a cosmetic
/// shape mismatch from failing the whole proposal parse (which would abort the consciousness cycle).
fn de_refs<'de, D>(d: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;
    let val_to_string = |v: &Value| -> String {
        match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    };
    Ok(match Value::deserialize(d)? {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), val_to_string(v))).collect(),
        Value::Array(items) => {
            items.iter().enumerate().map(|(i, v)| (i.to_string(), val_to_string(v))).collect()
        }
        Value::Null => BTreeMap::new(),
        other => BTreeMap::from([("0".to_string(), val_to_string(&other))]),
    })
}

// ── Model-boundary leniency (backported from dack v2, 2026-09-07) ────────────────────────────────
// Live v2 incidents proved every strict model-authored field is an eventual cycle-killer: a
// comma-joined string in `tags` and a tag_note missing `tag` each dropped WHOLE finished cycles
// (telegram replies silently lost). Leniency is the boundary default: repair what's usable, drop
// what isn't, never fail the cycle. Only a non-object output still errors (a real contract break).

/// Lenient scalar→string: the model writes ids it saw as numbers (`"reply_to": 3004`).
fn value_as_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Lenient `priority`: a hallucinated label (`"critical"`) degrades to `None` (inherit), never errors.
fn de_lenient_priority<'de, D>(d: D) -> Result<Option<Priority>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(d)?;
    Ok(raw.and_then(|s| match s.to_ascii_lowercase().as_str() {
        "low" => Some(Priority::Low),
        "normal" => Some(Priority::Normal),
        "high" => Some(Priority::High),
        "urgent" => Some(Priority::Urgent),
        _ => None,
    }))
}

/// Lenient `tags`: a comma-joined string splits; array items stringify scalars; garbage → none.
fn de_lenient_tags<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;
    Ok(match Value::deserialize(d)? {
        Value::String(s) => s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect(),
        Value::Array(items) => items.iter().filter_map(value_as_string).filter(|t| !t.is_empty()).collect(),
        n @ Value::Number(_) => vec![n.to_string()],
        _ => Vec::new(),
    })
}

/// Lenient `reply_to`: a numeric message id coerces to the string the harness matches on.
fn de_lenient_reply_to<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(value_as_string(&serde_json::Value::deserialize(d)?).filter(|s| !s.is_empty()))
}

/// Lenient `batons`: per-item salvage — an unusable baton (e.g. missing `to_prompt`) drops ALONE.
fn de_lenient_batons<'de, D>(d: D) -> Result<Vec<BatonIntent>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;
    let items: Vec<Value> = match Value::deserialize(d)? {
        Value::Array(items) => items,
        obj @ Value::Object(_) => vec![obj],
        _ => Vec::new(),
    };
    let total = items.len();
    let kept: Vec<BatonIntent> = items.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect();
    if kept.len() < total {
        tracing::warn!("batons: dropped {} of {total} malformed baton(s) (model shape drift)", total - kept.len());
    }
    Ok(kept)
}

/// Lenient `tag_notes`: a malformed note drops alone (a sticky note is disposable, the cycle is not).
fn de_lenient_tag_notes<'de, D>(d: D) -> Result<Option<Vec<TagNote>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;
    let to_note = |v: &Value| -> Option<TagNote> {
        let map = v.as_object()?;
        let tag = map.get("tag").and_then(value_as_string).filter(|s| !s.is_empty())?;
        let note = map.get("note").and_then(value_as_string).filter(|s| !s.is_empty())?;
        Some(TagNote { tag, note })
    };
    let items: Vec<Value> = match Value::deserialize(d)? {
        Value::Null => return Ok(None),
        Value::Array(items) => items,
        obj @ Value::Object(_) => vec![obj],
        _ => Vec::new(),
    };
    let total = items.len();
    let kept: Vec<TagNote> = items.iter().filter_map(to_note).collect();
    if kept.len() < total {
        tracing::warn!("tag_notes: dropped {} of {total} malformed note(s) (model shape drift)", total - kept.len());
    }
    Ok(if kept.is_empty() { None } else { Some(kept) })
}

/// Lenient salvage for an optional sub-object (`proposal`, `spawn`): malformed → `None` + warn.
fn de_lenient_opt<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = serde_json::Value::deserialize(d)?;
    if v.is_null() {
        return Ok(None);
    }
    Ok(match serde_json::from_value::<T>(v) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!("model output: dropped malformed optional sub-object ({e})");
            None
        }
    })
}

/// Lenient `transition`: object, or a bare string (`"transition": "express"`), or terminate.
fn de_lenient_transition<'de, D>(d: D) -> Result<Transition, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;
    Ok(match Value::deserialize(d)? {
        Value::String(s) if !s.is_empty() => Transition { to_prompt: Some(s), reason: String::new() },
        v @ Value::Object(_) => serde_json::from_value(v).unwrap_or_else(|e| {
            tracing::warn!("transition: malformed object degraded to terminate ({e})");
            Transition::default()
        }),
        _ => Transition::default(),
    })
}

/// Lenient `thought`: null/absent → empty; a scalar stringifies. A log line must not kill a cycle.
fn de_lenient_thought<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(value_as_string(&v).unwrap_or_default())
}

/// A requested state transition. The agent names the **next state-prompt id** it chooses
/// — exactly one of the current prompt's declared `transitions` (or `None` to terminate). Whether
/// it is honored is decided by the harness: the id must be in the allowed set, resolve to a real
/// `prompts/<id>.md`, sit within the route ceiling, and pass [`crate::state::allowed_transition`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Transition {
    /// `None` = terminate this cycle. Otherwise the chosen next state-prompt id (e.g.
    /// `twitter/feed_reply`).
    #[serde(default)]
    pub to_prompt: Option<String>,
    #[serde(default)]
    pub reason: String,
}

/// One **fan-out branch** the model proposes from a state. A single cycle may emit
/// SEVERAL — each its own digested gist + destination state-prompt, processed as an independent
/// branch with its own taint trajectory (the in-wake worklist). Supersedes the single [`Transition`]
/// (still accepted and normalized to a one-element fan-out by [`AgentOutput::fan_out`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatonIntent {
    /// The next state-prompt id this branch continues to — must be one of the EMITTING prompt's
    /// declared `transitions:` (and within the branch's trust ceiling; the harness re-checks both).
    pub to_prompt: String,
    /// The digested intent for THIS branch — the agent's OWN product (the firebreak), never raw
    /// stimulus text. Empty ⇒ the harness falls back to the proposal gist / thought (the legacy
    /// single-baton behaviour), so an old-shape output keeps working.
    #[serde(default)]
    pub gist: String,
    /// Proposed scheduling priority for this branch. Harness-CLAMPED (never trusted to raise above
    /// the origin) — `None` ⇒ inherit the cycle's. LENIENT: a hallucinated label degrades to `None`.
    #[serde(default, deserialize_with = "de_lenient_priority")]
    pub priority: Option<Priority>,
    /// The message this branch REPLIES TO — the id the model copies from a message it saw in the
    /// batch (`payload.items`), e.g. a telegram `message_id`. The harness VALIDATES it against the
    /// batch it holds (the firebreak): an id not in `items` is ignored (the reply falls back to the
    /// latest/top-level, never an arbitrary target). `None` = reply to the coalesced top-level (the
    /// latest message — legacy). Platform-agnostic; the identifier FIELD is the emitting prompt's
    /// `reply_key` (default `message_id`→`id`). LENIENT: a numeric id coerces to string.
    #[serde(default, deserialize_with = "de_lenient_reply_to")]
    pub reply_to: Option<String>,
    /// Extra context-recall tags for this branch (beyond the auto conversation key the harness adds when
    /// the prompt sets `tag_key`) — e.g. a topic. Carried onto the runlog entry so a tagged view can
    /// recall it later. Usually empty; the model rarely needs to set it. LENIENT: a comma-joined
    /// string splits instead of failing the cycle (the v2 six-lost-replies regression).
    #[serde(default, deserialize_with = "de_lenient_tags")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub reason: String,
}

/// A request to delegate a job to a **keyless sandboxed worker**. The agent names an
/// `agents/<agent>.md` def + a one-shot `brief`; the harness launches the worker ASYNCHRONOUSLY
/// (its own worker-spec sandbox, no soul/post/settle), and its summary returns later as an UNTRUSTED
/// `worker_completion` stimulus (the return-firebreak). Honored only from an act state (Express) —
/// the duck delegates, it does not become the worker. NOT the SDK `Task` tool (that's worker-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// The `agents/<agent>.md` definition to run (must resolve in the soul repo).
    pub agent: String,
    /// The task brief handed to the worker (untrusted-on-return; the duck decides what to publish).
    pub brief: String,
}

/// A "sticky note" on a conversation tag — a short observation appended to the short-term tag-notes
/// catalogue. The model returns just `{ tag, note }`; the HARNESS stamps `trust` (= the writing cycle's
/// taint, unfakeable provenance) and the `timestamp` on append.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagNote {
    pub tag: String,
    pub note: String,
}

/// The full agent return value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentOutput {
    /// Internal reasoning — **logged, never published** (Eliza-style "thought").
    /// Rides the Baton for continuity but is NOT a safety boundary.
    #[serde(default, deserialize_with = "de_lenient_thought")]
    pub thought: String,
    /// Tag-notes: per-conversation "sticky notes" appended to the SHORT-TERM tag-notes catalogue
    /// (`runlogs/tag-notes.ndjson`). Replaces the old `memory_append` — long-term `memory/` is written
    /// only by org+ digest jobs / reflect. Honored only for org+ (`memory_write_min_trust`)
    /// Express/Reflect cycles; dropped otherwise. `None` (the common case) = no note this cycle.
    /// LENIENT: a malformed note drops alone, never the cycle.
    #[serde(default, deserialize_with = "de_lenient_tag_notes")]
    pub tag_notes: Option<Vec<TagNote>>,
    #[serde(default, deserialize_with = "de_lenient_opt")]
    pub proposal: Option<Proposal>,
    /// Optional worker delegation. Honored only from Express; launched async, returns as
    /// an untrusted `worker_completion` stimulus. `None` (the common case) = no delegation.
    #[serde(default, deserialize_with = "de_lenient_opt")]
    pub spawn: Option<SpawnRequest>,
    /// Legacy single transition (still accepted). `fan_out()` folds it into `batons`.
    #[serde(default, deserialize_with = "de_lenient_transition")]
    pub transition: Transition,
    /// **Fan-out**: the branches this cycle wants to take. Several = do several things at
    /// once, each its own gist + destination, each an independent branch. Empty = terminate. Wins
    /// over the legacy `transition` when present. LENIENT: a malformed baton drops alone.
    #[serde(default, deserialize_with = "de_lenient_batons")]
    pub batons: Vec<BatonIntent>,
}

impl AgentOutput {
    /// The branches to fan out to, normalizing the legacy single `transition` into a one-element
    /// list: `batons` when present; else a `transition.to_prompt` becomes one branch; else empty
    /// (terminate). A synthesized branch carries an empty `gist` so the harness falls back to the
    /// proposal/thought (preserving the exact legacy single-baton payload).
    pub fn fan_out(&self) -> Vec<BatonIntent> {
        if !self.batons.is_empty() {
            return self.batons.clone();
        }
        match &self.transition.to_prompt {
            Some(id) => vec![BatonIntent {
                to_prompt: id.clone(),
                reason: self.transition.reason.clone(),
                ..Default::default()
            }],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v2-backported leniency: no model-authored field shape may drop a finished cycle. Replays the
    /// live v2 incident shapes (string tags, numeric reply_to, notes missing `tag`) + drift cases.
    #[test]
    fn model_output_survives_shape_drift_on_every_field() {
        use crate::model::stimulus::Priority;
        // The v2 six-lost-replies shape: string tags + numeric reply_to — REPAIRED, not dropped.
        let out: AgentOutput = serde_json::from_value(serde_json::json!({
            "thought": "guest asks wifi",
            "tag_notes": [{"tag": "chat-42", "note": "asked wifi"}, {"note": "orphan, no tag"}],
            "batons": [
                {"to_prompt": "express", "gist": "answer", "tags": "chat-42, wifi", "reply_to": 3004, "priority": "critical"},
                {"gist": "no to_prompt — drops alone"}
            ]
        }))
        .expect("no field shape may fail the parse");
        assert_eq!(out.tag_notes.as_ref().unwrap().len(), 1, "malformed note drops alone");
        let b = out.fan_out();
        assert_eq!(b.len(), 1, "unusable baton drops alone; the reply baton survives");
        assert_eq!(b[0].tags, vec!["chat-42", "wifi"], "comma-joined string splits");
        assert_eq!(b[0].reply_to.as_deref(), Some("3004"), "numeric reply_to coerces");
        assert_eq!(b[0].priority, None, "hallucinated priority degrades to inherit");
        let good: BatonIntent = serde_json::from_value(serde_json::json!({"to_prompt":"x","priority":"high"})).unwrap();
        assert_eq!(good.priority, Some(Priority::High));

        // Every remaining field degrades rather than errors.
        let out: AgentOutput = serde_json::from_value(serde_json::json!({
            "thought": null,
            "proposal": {"gist": "no intent"},
            "spawn": {"agent": "researcher"},
            "transition": "express",
            "batons": "not a list"
        }))
        .unwrap();
        assert_eq!(out.thought, "");
        assert!(out.proposal.is_none());
        assert!(out.spawn.is_none());
        assert_eq!(out.transition.to_prompt.as_deref(), Some("express"));
        assert!(out.batons.is_empty());

        // Ceiling: a non-object output (real contract break) still errors.
        assert!(serde_json::from_value::<AgentOutput>(serde_json::json!("just text")).is_err());
    }

    /// A weaker model emits `intent`/`refs` in shapes the strict schema would reject. None of these
    /// may abort the cycle: an unknown intent degrades to `Other`, and `refs` accepts any JSON shape.
    #[test]
    fn proposal_parse_tolerates_model_shape_drift() {
        // refs as an ARRAY (the mimo shape that broke the live worker demo), unknown intent label.
        let p: Proposal = serde_json::from_str(
            r#"{"intent":"build","gist":"g","refs":["standing-directive: x","ref two"]}"#,
        )
        .expect("array refs + unknown intent must parse");
        assert_eq!(p.intent, Intent::Other);
        assert_eq!(p.refs.get("0").map(String::as_str), Some("standing-directive: x"));
        assert_eq!(p.refs.get("1").map(String::as_str), Some("ref two"));

        // refs as an OBJECT still works; a known intent still maps.
        let p: Proposal =
            serde_json::from_str(r#"{"intent":"delegate","gist":"g","refs":{"k":"v"}}"#).unwrap();
        assert_eq!(p.intent, Intent::Delegate);
        assert_eq!(p.refs.get("k").map(String::as_str), Some("v"));

        // refs null / omitted → empty (no error).
        let p: Proposal = serde_json::from_str(r#"{"intent":"noop","gist":"g","refs":null}"#).unwrap();
        assert!(p.refs.is_empty());
        let p: Proposal = serde_json::from_str(r#"{"intent":"noop","gist":"g"}"#).unwrap();
        assert!(p.refs.is_empty());
    }

    /// `fan_out()` normalizes both shapes: the new `batons` list wins; a legacy single `transition`
    /// folds to one branch; nothing ⇒ terminate. This is the back-compat contract for .
    #[test]
    fn fan_out_normalizes_legacy_and_multi() {
        // New shape: an explicit batons list is used verbatim (the fan-out).
        let multi: AgentOutput = serde_json::from_str(
            r#"{"thought":"t","batons":[
                 {"to_prompt":"telegram/express","gist":"reply","reply_to":"42"},
                 {"to_prompt":"settle","gist":"trade","priority":"high"}]}"#,
        )
        .unwrap();
        let b = multi.fan_out();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].to_prompt, "telegram/express");
        assert_eq!(b[0].reply_to.as_deref(), Some("42"), "reply target parses");
        assert_eq!(b[1].to_prompt, "settle");
        assert!(b[1].reply_to.is_none());
        assert!(matches!(b[1].priority, Some(crate::model::stimulus::Priority::High)));

        // Legacy shape: a single transition folds to exactly one branch (empty gist → harness
        // falls back to proposal/thought when building the baton).
        let legacy: AgentOutput = serde_json::from_str(
            r#"{"thought":"t","proposal":{"intent":"reply","gist":"g"},
                "transition":{"to_prompt":"express","reason":"r"}}"#,
        )
        .unwrap();
        let b = legacy.fan_out();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].to_prompt, "express");
        assert!(b[0].gist.is_empty(), "synthesized branch defers gist to the harness");
        assert!(b[0].reply_to.is_none(), "legacy transition has no reply target");

        // Terminate: no batons, null transition ⇒ no branches.
        let term: AgentOutput =
            serde_json::from_str(r#"{"thought":"t","transition":{"to_prompt":null}}"#).unwrap();
        assert!(term.fan_out().is_empty());
        // Default output also terminates (the ScriptedRuntime's terminal value).
        assert!(AgentOutput::default().fan_out().is_empty());
    }
}
