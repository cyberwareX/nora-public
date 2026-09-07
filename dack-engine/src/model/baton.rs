//! The **Baton** — the firebreak made structural.
//!
//! The one piece of data that is neither cleanly harness-owned nor agent-owned: it
//! is *in flight* between two agent invocations, mediated by the harness. The
//! governing rule: the Baton carries the agent's **own digested products** plus the
//! harness's **trusted provenance metadata** — never the raw untrusted bytes. Raw
//! untrusted content is reachable only *by reference* (`runlog_ref`), only
//! *framed-as-untrusted*, never *auto-inlined* into the tool-bearing context.
//!
//! What it covers: contamination *carried across* (verbatim "ignore previous
//! instructions…" never rides into Express). What it does NOT cover: a malicious
//! *conclusion* carried across (the laundered-plan attack) — that residual class is
//! caught only by bounded blast radius and the irreversibility wall, never
//! by context reset. Do not let this drift to over-confidence.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::stimulus::TrustTier;

/// Perceive → Express/Settle handoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baton {
    /// Digested intent — the agent's OWN product, **not** raw stimulus text. The
    /// firebreak's core: poisoned text becomes a safe-shaped intent or it does not
    /// survive digestion.
    pub gist: String,
    /// Structured references the next state needs to act (e.g. `in_reply_to`).
    pub refs: BTreeMap<String, String>,
    /// Trust of the standing duty that fired (harness-authored annotation).
    pub directive_tier: TrustTier,
    /// Trust of the world-data digested into the gist (harness-authored annotation).
    /// Lets Express be more skeptical of a gist whose payload was `public`.
    pub payload_tier: TrustTier,
    /// Pointer, NOT inlined content. Express may *choose* to read the runlog (which
    /// stores raw stimulus framed-untrusted). Reading framed material by choice is
    /// fine; auto-inlining raw bytes unframed is what breaks the firebreak.
    pub runlog_ref: String,
    /// Perceive's reasoning, carried for continuity/coherence only.
    ///
    /// CAVEAT (do not mistake for a check): a laundered conclusion lives *inside* the
    /// thought. Express seeing it aids coherence; it does NOT let Express catch a
    /// laundered conclusion. Carry it; never treat it as a safety boundary.
    pub thoughts: String,
    /// Context-recall tags this branch carries (from the emitting `BatonIntent.tags`) — written onto the
    /// Express runlog entry alongside the auto conversation key. Usually empty.
    #[serde(default)]
    pub tags: Vec<String>,
}
