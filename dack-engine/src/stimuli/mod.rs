//! The stimulus registry. **The `stimuli/` directory IS the agent's
//! job description**: each `.md` is a *standing duty* the agent performs when triggered,
//! carrying its own briefing text. A set of duty-stimuli is a decomposed job
//! description — the unit that generalizes to future firm actors.
//!
//! Registration convention: walk `stimuli/` to **max depth 2**; every `.md` (depth ≤ 2)
//! is a stimulus definition. Sensors live in a sibling `scripts/`. The `.md`'s
//! **frontmatter is config**; its **body is the directive text** (trusted).
//! `stimuli/` is writable only in Reflect — so this is gated self-evolution of the
//! duck's own senses and duties.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::CoalescePolicy;

/// Default entry state-prompt id for a duty that omits `entry:` — the flat `prompts/perceive.md`.
fn default_entry_id() -> String {
    "perceive".to_string()
}
use crate::error::{DackError, Result};
use crate::model::stimulus::{Priority, StimulusType, TrustTier};

/// Trigger that fires a duty. `cron` (re)schedules a timer; `webhook`
/// registers a listener on the fly. The Twitter push-path swap is just
/// flipping `cron` → `webhook` here — no architectural change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Trigger {
    Cron { schedule: String },
    Webhook { path: String },
}

/// What rows this duty emits (`emits:`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Emits {
    #[serde(rename = "type")]
    pub type_: StimulusType,
}

/// Cross-poll dedup cursor — a monotonic **watermark** the harness persists in the
/// queue DB so a polling sensor never re-discovers what it already saw (e.g. X `since_id`). The
/// harness fetches the stored value, injects it into the sensor's env before the run, and after
/// the run advances it to the max `field` over the discovered candidates. Single-flight → no race.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorSpec {
    /// Candidate-payload field whose MAX is the new watermark (e.g. `id` — a Twitter snowflake,
    /// monotonic). Compared numerically when both values parse as integers, else lexically.
    pub field: String,
    /// Env var the stored watermark is injected into the sensor as (e.g. `DACK_SINCE_ID`). Absent
    /// on the first poll (no stored value yet) — the sensor then fetches from the beginning.
    pub env: String,
    /// DB key the watermark is stored under. Defaults to the duty id (one cursor per duty).
    #[serde(default)]
    pub key: Option<String>,
}

impl CursorSpec {
    /// The DB key for this duty's watermark (explicit `key`, else the duty id).
    pub fn key_for(&self, duty_id: &str) -> String {
        self.key.clone().unwrap_or_else(|| duty_id.to_string())
    }
}

/// The YAML frontmatter of a `STIMULUS.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StimulusFrontmatter {
    pub id: String,
    pub trigger: Trigger,
    /// Path to the sensor executable (resolved into the sibling `scripts/`). Omit for a
    /// pure-cron self-prompt. **Honored only for trusted directive tiers**.
    #[serde(default)]
    pub sensor: Option<String>,
    /// Trust tier of THIS `.md` body (trusted intent): `self` | `operator_signed`.
    pub directive_tier: TrustTier,
    pub emits: Emits,
    #[serde(default)]
    pub coalesce: Option<CoalescePolicy>,
    /// The **entry state-prompt id** this duty opens at a path under `prompts/` without
    /// the extension, e.g. `twitter/perceive_mention` or the flat `perceive`. The soul owns the
    /// chain from here (the prompt's `transitions`); the operator route only sets the ceiling.
    #[serde(default = "default_entry_id")]
    pub entry: String,
    #[serde(default)]
    pub priority: Option<Priority>,
    /// **Dispatch window**: a daily UTC range `"HH:MM-HH:MM"` (may cross midnight, e.g.
    /// `"22:00-05:00"`). A stimulus arriving OUTSIDE it is held — via `pop_after` — until the window
    /// next opens: "handle the noisy public groups only at deep night." `None` ⇒ always eligible.
    #[serde(default)]
    pub dispatch_window: Option<String>,
    /// Secrets-provider scopes this duty's sensor needs (by provider `name`). The harness
    /// runs those trusted provider scripts and injects only their short-lived token env — the
    /// sensor never holds the root credential (`docs/SECRETS-AND-SANDBOX.md`).
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Optional cross-poll dedup watermark — see [`CursorSpec`]. Present on polling
    /// sensors (mentions) so the duck never re-processes (or re-replies to) an already-seen item.
    #[serde(default)]
    pub cursor: Option<CursorSpec>,
}

/// A daily **dispatch window** in UTC, parsed from `"HH:MM-HH:MM"`. A stimulus arriving
/// outside the window is deferred — via the queue's `pop_after` gate — to the next time it opens.
/// Crosses midnight when `start > end` (e.g. `"22:00-05:00"`). UTC keeps it deterministic; the
/// operator picks the range in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchWindow {
    start_min: u32,
    end_min: u32,
}

impl DispatchWindow {
    /// Parse `"HH:MM-HH:MM"` (24h, UTC). `None` on any malformed/degenerate (start == end) input.
    pub fn parse(s: &str) -> Option<Self> {
        let (a, b) = s.split_once('-')?;
        let parse_min = |x: &str| -> Option<u32> {
            let (h, m) = x.trim().split_once(':')?;
            let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
            (h < 24 && m < 60).then_some(h * 60 + m)
        };
        let (start_min, end_min) = (parse_min(a)?, parse_min(b)?);
        (start_min != end_min).then_some(Self { start_min, end_min })
    }

    /// Whether `minute_of_day` (0..1440) is inside the window (handles the midnight wrap).
    pub fn contains(&self, minute_of_day: u32) -> bool {
        if self.start_min <= self.end_min {
            minute_of_day >= self.start_min && minute_of_day < self.end_min
        } else {
            minute_of_day >= self.start_min || minute_of_day < self.end_min
        }
    }

    /// The unix second at which this window is next OPEN for a stimulus arriving at `at` (UTC): `at`
    /// itself if already open, else the next occurrence of the window's start.
    pub fn next_open(&self, at: i64) -> i64 {
        const DAY: i64 = 86_400;
        let since_midnight = at.rem_euclid(DAY);
        if self.contains((since_midnight / 60) as u32) {
            return at;
        }
        let start_today = (at - since_midnight) + self.start_min as i64 * 60;
        if start_today > at {
            start_today
        } else {
            start_today + DAY
        }
    }
}

/// A registered duty: parsed frontmatter + the trusted directive body.
#[derive(Debug, Clone)]
pub struct StimulusDef {
    pub frontmatter: StimulusFrontmatter,
    /// The `.md` body — the trusted briefing appended to Perceive context.
    pub directive_body: String,
    /// Repo-relative path the def was loaded from (used for the trusted-dir check).
    pub source_path: String,
}

/// Split a `---`-delimited frontmatter document into (yaml, body). Returns an error if
/// the leading `---` fence is missing or unterminated.
pub fn split_frontmatter(text: &str) -> Result<(&str, &str)> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or_else(|| DackError::Stimulus("missing leading `---` frontmatter fence".into()))?;
    // Find the closing fence at the start of a line.
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.find("\n---\r\n"))
        .ok_or_else(|| DackError::Stimulus("unterminated frontmatter fence".into()))?;
    let yaml = &rest[..end];
    // Body starts after the closing fence line.
    let after = &rest[end..];
    let body = after
        .strip_prefix("\n---\n")
        .or_else(|| after.strip_prefix("\n---\r\n"))
        .unwrap_or("");
    Ok((yaml, body.trim_start()))
}

impl StimulusDef {
    /// Parse one `STIMULUS.md` from its raw text + repo path.
    pub fn parse(text: &str, source_path: impl Into<String>) -> Result<Self> {
        let (yaml, body) = split_frontmatter(text)?;
        let frontmatter: StimulusFrontmatter = serde_yaml::from_str(yaml)?;
        Ok(StimulusDef {
            frontmatter,
            directive_body: body.to_string(),
            source_path: source_path.into(),
        })
    }

    /// Whether this def's `sensor` reference is honored (trusted directive tier).
    /// The trusted-*dir* half is enforced at registration (only trusted repo dirs are
    /// walked); this is the tier half.
    pub fn sensor_honored(&self) -> bool {
        crate::sensor::scripts_honored(&self.frontmatter.directive_tier)
    }

    /// Absolute path to this duty's sensor executable, if it declares one **and** its
    /// directive tier honors scripts. Resolved relative to the duty's own
    /// directory (the dir holding the `.md`), so `sensor: ./scripts/x.sh` lands in the
    /// sibling `scripts/`. `None` for pure-cron self-prompts or untrusted tiers.
    pub fn resolved_sensor(&self, repo_root: impl AsRef<Path>) -> Option<PathBuf> {
        if !self.sensor_honored() {
            return None;
        }
        let rel = self.frontmatter.sensor.as_ref()?;
        let md_abs = repo_root.as_ref().join(&self.source_path);
        Some(md_abs.parent()?.join(rel))
    }
}

/// Maximum file-depth the registry walks under `stimuli/`: a file directly in
/// `stimuli/` is depth 1, so `MAX_DEPTH = 2` admits the `stimuli/<duty>/STIMULUS.md`
/// convention and excludes anything more deeply nested.
pub const MAX_DEPTH: usize = 2;

/// The registered set of duties — **the agent's job description** — plus any
/// per-file parse errors. A malformed duty is **skipped, not fatal**: one bad `.md` must
/// not blind the duck to all its other senses (logging-not-rollback). The
/// registry is rebuilt on each `stimuli/` change (hot-reload).
#[derive(Debug, Default)]
pub struct Registry {
    pub defs: Vec<StimulusDef>,
    /// `(repo-relative path, error)` for each `.md` that failed to parse.
    pub errors: Vec<(String, String)>,
}

impl Registry {
    /// Walk `<repo_root>/stimuli/` to file-depth ≤ [`MAX_DEPTH`], parsing every `.md`.
    /// Only `stimuli/` is walked — the **trusted-dir** half of the sensor gate:
    /// nothing outside the duck's own repo can register a duty or a script.
    pub fn load(repo_root: impl AsRef<Path>) -> Result<Registry> {
        let repo_root = repo_root.as_ref();
        let mut reg = Registry::default();
        let stimuli = repo_root.join("stimuli");
        if stimuli.is_dir() {
            reg.walk(&stimuli, repo_root, 1);
        }
        Ok(reg)
    }

    fn walk(&mut self, dir: &Path, repo_root: &Path, file_depth: usize) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                self.errors.push((rel(dir, repo_root), e.to_string()));
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if file_depth < MAX_DEPTH {
                    self.walk(&path, repo_root, file_depth + 1);
                }
            } else if file_depth <= MAX_DEPTH
                && path.extension().and_then(|e| e.to_str()) == Some("md")
            {
                let source_path = rel(&path, repo_root);
                match std::fs::read_to_string(&path)
                    .map_err(DackError::from)
                    .and_then(|t| StimulusDef::parse(&t, source_path.clone()))
                {
                    Ok(def) => self.defs.push(def),
                    Err(e) => self.errors.push((source_path, e.to_string())),
                }
            }
        }
    }

    pub fn get(&self, def_id: &str) -> Option<&StimulusDef> {
        self.defs.iter().find(|d| d.frontmatter.id == def_id)
    }

    /// `(def_id, cron_schedule)` for every cron-triggered duty — feeds the scheduler.
    pub fn cron_routes(&self) -> Vec<(String, String)> {
        self.defs
            .iter()
            .filter_map(|d| match &d.frontmatter.trigger {
                Trigger::Cron { schedule } => Some((d.frontmatter.id.clone(), schedule.clone())),
                Trigger::Webhook { .. } => None,
            })
            .collect()
    }

    /// `(path, def_id)` for every webhook-triggered duty — feeds the listener.
    pub fn webhook_routes(&self) -> Vec<(String, String)> {
        self.defs
            .iter()
            .filter_map(|d| match &d.frontmatter.trigger {
                Trigger::Webhook { path } => Some((path.clone(), d.frontmatter.id.clone())),
                Trigger::Cron { .. } => None,
            })
            .collect()
    }
}

/// Repo-relative, forward-slashed path string (the `source_path` form defs are keyed by).
fn rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_window_parse_contains_and_next_open() {
        // 02:00–05:00 UTC (a normal, non-wrapping window).
        let w = DispatchWindow::parse("02:00-05:00").unwrap();
        assert!(w.contains(3 * 60) && !w.contains(1 * 60) && !w.contains(6 * 60));
        assert_eq!(w.next_open(0), 7_200, "midnight → held until 02:00 today");
        assert_eq!(w.next_open(10_000), 10_000, "inside the window → open now");
        assert_eq!(w.next_open(43_200), 86_400 + 7_200, "noon → 02:00 tomorrow");

        // 22:00–05:00 UTC (wraps midnight).
        let w = DispatchWindow::parse("22:00-05:00").unwrap();
        assert!(w.contains(23 * 60) && w.contains(3 * 60) && !w.contains(12 * 60));
        assert_eq!(w.next_open(43_200), 22 * 3_600, "noon → 22:00 today");
        assert_eq!(w.next_open(23 * 3_600), 23 * 3_600, "23:00 is inside → now");

        // Malformed / degenerate → None.
        assert!(DispatchWindow::parse("nope").is_none());
        assert!(DispatchWindow::parse("25:00-05:00").is_none());
        assert!(DispatchWindow::parse("02:00-02:00").is_none());
    }

    // The  worked example, verbatim shape.
    const CLARITY: &str = r#"---
id: clarity-reply-guy
trigger: { type: cron, schedule: "0 * * * *" }
sensor: ./scripts/fetch_clarity_posts.sh
directive_tier: self
emits:
  type: clarity_post
  default_payload_tier: public
coalesce: { mode: batch, window_sec: 600, dedup_key: tweet_id }
entry: perceive
priority: low
---
Standing directive (trusted): survey new posts discussing the CLARITY act and engage
as a reply-guy, framing DAC's ministerial-management model as the timely fit. Be
selective; skip low-quality bait.
"#;

    #[test]
    fn parses_the_clarity_reply_guy_duty() {
        let def = StimulusDef::parse(CLARITY, "stimuli/clarity-reply-guy/STIMULUS.md").unwrap();
        assert_eq!(def.frontmatter.id, "clarity-reply-guy");
        assert_eq!(def.frontmatter.directive_tier, TrustTier::self_());
        assert_eq!(def.frontmatter.emits.type_, StimulusType::from("clarity_post"));
        assert!(matches!(def.frontmatter.trigger, Trigger::Cron { .. }));
        assert!(def.frontmatter.sensor.is_some());
        // self-tier directive → sensor honored.
        assert!(def.sensor_honored());
        // Directive body is the trusted briefing, carried separately from payload.
        assert!(def.directive_body.starts_with("Standing directive (trusted):"));
    }

    #[test]
    fn rejects_missing_frontmatter() {
        assert!(StimulusDef::parse("no frontmatter here", "x.md").is_err());
    }

    // ── Registry / walker ───────────────────────────────────────────

    fn cron_duty(id: &str, sensor: Option<&str>) -> String {
        let sensor_line = sensor.map(|s| format!("sensor: {s}\n")).unwrap_or_default();
        format!(
            "---\nid: {id}\ntrigger: {{ type: cron, schedule: \"0 * * * *\" }}\n{sensor_line}\
             directive_tier: self\nemits:\n  type: t_{id}\n  default_payload_tier: public\n\
             entry: perceive\n---\nDirective for {id}.\n"
        )
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn temp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dack-reg-{}-{}", tag, std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn walks_duties_skips_malformed_and_resolves_sensor() {
        let root = temp_repo("walk");
        write(&root, "stimuli/clarity/STIMULUS.md", &cron_duty("clarity", Some("./scripts/fetch.sh")));
        write(&root, "stimuli/heartbeat/STIMULUS.md", &cron_duty("heartbeat", None));
        write(&root, "stimuli/broken/STIMULUS.md", "no frontmatter at all");
        // Too deep (file-depth 3) — must NOT be registered.
        write(&root, "stimuli/a/b/DEEP.md", &cron_duty("deep", None));

        let reg = Registry::load(&root).unwrap();
        assert_eq!(reg.defs.len(), 2, "two valid duties");
        assert_eq!(reg.errors.len(), 1, "broken one skipped + recorded");
        assert!(reg.errors[0].0.contains("broken"));
        assert!(reg.get("deep").is_none(), "depth>2 excluded");

        // Sensor resolves into the duty's sibling scripts/.
        let clarity = reg.get("clarity").unwrap();
        let sensor = clarity.resolved_sensor(&root).unwrap();
        assert!(sensor.ends_with("stimuli/clarity/scripts/fetch.sh"), "{sensor:?}");
        // Pure-cron duty has no sensor.
        assert!(reg.get("heartbeat").unwrap().resolved_sensor(&root).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn derives_cron_and_webhook_routes() {
        let root = temp_repo("routes");
        write(&root, "stimuli/poller/STIMULUS.md", &cron_duty("poller", None));
        write(
            &root,
            "stimuli/inbox/STIMULUS.md",
            "---\nid: inbox\ntrigger: { type: webhook, path: /hooks/inbox }\n\
             directive_tier: self\nemits:\n  type: msg\n  default_payload_tier: public\n\
             entry: perceive\n---\nInbound webhook duty.\n",
        );
        let reg = Registry::load(&root).unwrap();

        let crons = reg.cron_routes();
        assert_eq!(crons, vec![("poller".to_string(), "0 * * * *".to_string())]);
        let hooks = reg.webhook_routes();
        assert_eq!(hooks, vec![("/hooks/inbox".to_string(), "inbox".to_string())]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_stimuli_dir_is_empty_not_error() {
        let root = temp_repo("empty");
        let reg = Registry::load(&root).unwrap();
        assert!(reg.defs.is_empty() && reg.errors.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
