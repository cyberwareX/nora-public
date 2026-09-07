//! Operator config — the non-LLM authority surface.
//!
//! A rich YAML the operator owns and the harness reads (hot-reloadable). Holds the trust lattice
//! (the taint model), the MCP registry + per-tier policy, secrets-provider refs, the reflect
//! rate-limit, forwarded-env *names*, secret *references* (never values), and the trusted operator
//! DID. The agent may *read* select fields (it should know it can be killed) but can **never
//! write** this — it is operator writer-of-record.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{DackError, Result};
use crate::model::stimulus::TrustTier;
use crate::state::ConsciousnessState;

/// Coalescing policy — both an economics knob and a character decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoalescePolicy {
    pub mode: CoalesceMode,
    #[serde(default)]
    pub window_sec: Option<u64>,
    #[serde(default)]
    pub dedup_key: Option<String>,
    /// Adaptive (`batch` only): instead of a flat `window_sec`, grow the coalesce window as each chat
    /// spends a daily credit budget — snappy while there's budget, gracefully slower as a chat gets
    /// noisy, with a per-chat daily cost ceiling. When set, `window_sec` is ignored. Tuned by the soul
    /// in Reflect (responsiveness is a knob the duck owns).
    #[serde(default)]
    pub adaptive: Option<AdaptiveCoalesce>,
}

/// Per-chat adaptive coalesce budget. A *credit* = one wake (one new batch accumulator). The window
/// grows as a chat spends its daily credits: every time it burns half its REMAINING credits, the
/// window ×10, capped at `max_window_sec`. `step = floor(log2(daily_credits / remaining))`,
/// `window = min(initial_window_sec · 10^step, max_window_sec)`; underwater (remaining ≤ 0) ⇒ the cap.
/// So a normal chat never leaves `initial_window_sec`; only a noisy one throttles itself. Per chat
/// (`dedup_key`), reset daily. (A regenerating GLOBAL pool per stimulus is a deferred follow-up.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveCoalesce {
    pub initial_window_sec: u64,
    pub daily_credits: u64,
    pub max_window_sec: u64,
}

impl AdaptiveCoalesce {
    /// The coalesce window (seconds) given how many credits this chat has already spent today.
    pub fn window_sec(&self, consumed: u64) -> u64 {
        let remaining = self.daily_credits.saturating_sub(consumed);
        if remaining == 0 {
            return self.max_window_sec;
        }
        // step = number of times the budget has halved = floor(log2(daily_credits / remaining)).
        let step = ((self.daily_credits as f64 / remaining as f64).log2()).floor().max(0.0) as u32;
        self.initial_window_sec
            .saturating_mul(10u64.saturating_pow(step))
            .min(self.max_window_sec)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoalesceMode {
    /// Batch within a window keyed by `dedup_key` (50 mentions → 1 wake).
    Batch,
    /// Keep only the latest per `dedup_key`.
    Latest,
    /// One-at-a-time; each candidate judged individually (e.g. token launches).
    None,
}

// The `(payload_tier, type)` routing table is GONE. The transition ceiling is now derived
// from the taint model (`TrustLattice::reaches`); `priority` + `coalesce` live in the stimulus
// frontmatter; the `entry` state-prompt is the stimulus's own `entry:`. Capability grants are the
// `tier_policy` handshake. Nothing remains to route by an agent-self-assigned `type`.

/// Per-consciousness-tier MCP policy — the OPERATOR's half of the
/// capability handshake. A state-prompt's `mcp:` requests are admitted only by this policy:
/// - `import`: the operator-registered `mcp_servers` a state-prompt at this tier may import by name
///   (its token is injected server-side, never the agent context). The allowlist of secret-bearing
///   capabilities reachable here.
/// - `mcp_whitelist`: when `true` (the safe default, like Settle) ONLY `import` refs are allowed —
///   no self-plug. When `false` (an open tier, e.g. Perceive) the soul may ALSO inline ANY public,
///   secret-less MCP `{name,url}` (forced read-tier — a soul can never self-grant post/settle).
/// - `deny`: an explicit blocklist (belt-and-suspenders; a denied name is never admitted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierPolicy {
    /// `true` (default) = imports only (locked); `false` = the soul may also inline public read MCPs.
    #[serde(default = "default_true")]
    pub mcp_whitelist: bool,
    /// Operator-registered server names a state-prompt at this tier may import.
    #[serde(default)]
    pub import: Vec<String>,
    /// Server names never admitted at this tier (overrides `import` and inline).
    #[serde(default)]
    pub deny: Vec<String>,
    /// The OPERATOR's model for this tier — the default model a state-prompt at this tier runs on.
    /// `None` falls back to the global `config.model`. (The model handshake, mirroring `mcp_whitelist`.)
    #[serde(default)]
    pub model: Option<String>,
    /// `false` (default) = the operator's `model` (or global) is fixed for this tier; a state-prompt's
    /// own `model:` is ignored. `true` = the soul may self-select a per-prompt `model:` (e.g. a Reflect
    /// tier the operator lets pick a stronger model). Operator boundary; soul self-selects within it.
    #[serde(default)]
    pub allow_model_override: bool,
}

impl Default for TierPolicy {
    /// The safe default for an unconfigured tier: **nothing grantable** — imports-only (locked) with
    /// an empty import list (least privilege; the operator must explicitly open a tier). Model: the
    /// global default, no soul override.
    fn default() -> Self {
        Self {
            mcp_whitelist: true,
            import: Vec::new(),
            deny: Vec::new(),
            model: None,
            allow_model_override: false,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_log_level() -> String {
    "info".into()
}
fn default_log_format() -> String {
    "auto".into()
}

/// Operational logging config (the `tracing` subscriber). Distinct from the runlog (the agent's
/// soul-committed self-narrative). `level`: trace|debug|info|warn|error (env `RUST_LOG` overrides for
/// per-module control, e.g. `dack::harness=debug`). `format`: `auto` (pretty TEXT on a TTY, JSON when
/// piped/containerized — `docker logs`-parseable), or force `text`/`json`. Logs go to stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_format")]
    pub format: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { level: default_log_level(), format: default_log_format() }
    }
}

/// One tier in the operator-configured **trust lattice** (the taint/IFC model). A `name` plus the
/// highest consciousness state a cycle whose accumulated trust is this tier may walk to (`reaches`).
/// **Rank = position** in `DackConfig.trust_tiers` (low→high); the lattice is a total order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustTierDef {
    pub name: String,
    /// State ceiling for a cycle at this tier. `public→express`, `org→settle`, `self→reflect`, …
    #[serde(default = "default_reaches")]
    pub reaches: ConsciousnessState,
}

fn default_reaches() -> ConsciousnessState {
    ConsciousnessState::Express
}

fn default_memory_write_min_trust() -> String {
    "org".into()
}
fn default_tag_notes_max() -> usize {
    200
}
fn default_runlog_retention_days() -> u64 {
    10
}

fn default_reflect_interval() -> i64 {
    86_400 // one day — a sane self-modification cadence; the operator may loosen or `0` to disable.
}
fn default_queue_max_depth() -> Option<usize> {
    Some(10_000)
}
fn default_session_ttl() -> i64 {
    3_600 // one hour idle before a sticky session is dropped.
}

/// The resolved trust lattice — built from `DackConfig.trust_tiers` (or the safe default). A total
/// order by rank; the taint `meet` (greatest-lower-bound) is just the lower-ranked tier. An unknown
/// tier name fails SAFE (rank 0 / `reaches: express`) so a mislabel can never *raise* a ceiling.
#[derive(Debug, Clone)]
pub struct TrustLattice {
    tiers: Vec<TrustTierDef>, // index = rank
}

impl TrustLattice {
    /// Privilege rank (higher = more trusted). Unknown name → 0 (lowest), fail-safe.
    pub fn rank(&self, t: &TrustTier) -> usize {
        self.tiers.iter().position(|d| d.name == t.name()).unwrap_or(0)
    }
    /// The state ceiling a cycle at tier `t` may walk to. Unknown name → `Express` (safe).
    pub fn reaches(&self, t: &TrustTier) -> ConsciousnessState {
        self.tiers
            .iter()
            .find(|d| d.name == t.name())
            .map(|d| d.reaches)
            .unwrap_or(ConsciousnessState::Express)
    }
    /// The taint **meet**: the lower-trust (most-degraded) of two tiers. Monotonic down (I17).
    pub fn meet(&self, a: &TrustTier, b: &TrustTier) -> TrustTier {
        if self.rank(a) <= self.rank(b) {
            a.clone()
        } else {
            b.clone()
        }
    }
    /// The AUTHORIZATION check: does a cycle at trust `cycle` clear a capability's
    /// `min_trust`? True iff `cycle` ranks `>=` `min`. Unknown names rank 0 (fail-safe — an unknown
    /// cycle tier clears only rank-0 `min`; an unknown `min` is rank 0 so it gates nothing extra).
    pub fn permits(&self, cycle: &TrustTier, min: &TrustTier) -> bool {
        self.rank(cycle) >= self.rank(min)
    }
    pub fn contains(&self, name: &str) -> bool {
        self.tiers.iter().any(|d| d.name == name)
    }
}

/// The safe default lattice (when `trust_tiers:` is omitted) — reproduces the pre-taint behavior:
/// `public(→express) < self(→reflect) < operator_signed(→reflect)`.
fn default_trust_tiers() -> Vec<TrustTierDef> {
    use ConsciousnessState::*;
    vec![
        TrustTierDef { name: "public".into(), reaches: Express },
        TrustTierDef { name: "self".into(), reaches: Reflect },
        TrustTierDef { name: "operator_signed".into(), reaches: Reflect },
    ]
}

/// Lowercase-hex sha256 of `bytes` — the key into `signed_scripts` (operator code-review-as-signing).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}


/// One secrets provider (a trusted, harness-owned script) — see [`DackConfig::secrets_providers`].
/// The `command` is run with `env` (its config — e.g. creds-file/token-store paths) injected;
/// it prints a JSON object `{"ENV_KEY": "value", …}` on stdout that the harness injects into
/// the consumer that declared this provider's `name`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsProviderConfig {
    pub name: String,
    /// argv of the provider script, e.g. `["python3", "secrets-providers/x_oauth2.py"]`.
    pub command: Vec<String>,
    /// Config env passed to the script (paths/refs — NOT secret values).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Env keys this provider is expected to emit (documentation / validation; optional).
    #[serde(default)]
    pub keys: Vec<String>,
    /// **Trust tier** of the data this secret puts in play (the taint model). A sensor using this
    /// provider seeds its cycle at (at most) this tier — e.g. the X bearer touches the public world
    /// so `trust: public` caps any twitter cycle at Express. Default `self` (the operator's own).
    #[serde(default = "default_self_trust")]
    pub trust: TrustTier,
}

fn default_self_trust() -> TrustTier {
    TrustTier::self_()
}

/// Capability tier of an MCP server — maps its tools to a `ToolClass` and the
/// states that may reach them. The OPERATOR declares it; the agent can never change it (it would
/// be self-granting authority). This is the single source of truth the wall classifies from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityTier {
    /// Read-only (balances, prices, search). Safe in every state.
    Read,
    /// Reversible external effect (post/reply). Express only.
    Post,
    /// Irreversible authority (trade/transfer/vote). Settle only — reachable only by an
    /// uncontaminated cycle (the taint model); externally bounded by the custodial/wallet limits.
    Settle,
}

/// How an MCP capability server is reached.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpTransport {
    /// A remote streamable-HTTP MCP (e.g. `https://production.cove.trade/api/mcp`).
    Http { url: String },
    /// A locally-spawned stdio MCP (e.g. our own `twitter-mcp.ts`).
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

/// How the harness injects the materialized auth token into an MCP server — so the token reaches
/// the server but NEVER the agent's context (http: a request header; stdio: the server's env).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpAuth {
    /// Secrets-provider `name` whose materialized value is the token (operator-supplied).
    pub secret: String,
    /// Which env key the provider emits to use as the token. Defaults to the provider's sole key.
    #[serde(default)]
    pub key: Option<String>,
    /// HTTP: the header name to carry the token (default `Authorization`). A custom header like
    /// `X-API-Key` is supported for servers with non-standard auth.
    #[serde(default)]
    pub header: Option<String>,
    /// HTTP: the prefix before the token, e.g. `Authorization: Bearer <tok>`. Default is HEADER-AWARE:
    /// `Bearer` for the `Authorization` header, RAW (no prefix → `X-API-Key: <tok>`) for any other
    /// header. Set explicitly to override — e.g. `Token`, or `""` to force a raw token.
    #[serde(default)]
    pub scheme: Option<String>,
    /// stdio: inject the token as this env var in the spawned server process.
    #[serde(default)]
    pub env: Option<String>,
}

/// An operator-declared MCP **capability server**. The registry is OPERATOR authority:
/// adding cove.trade, the next tool, … is a config entry — never a harness/bridge code change.
/// `tier` + the wall gate what its tools may do and which state reaches them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Server name → tool prefix `mcp__<name>__`. The wall classifies by this prefix + `tier`.
    pub name: String,
    pub transport: McpTransport,
    #[serde(default)]
    pub auth: Option<McpAuth>,
    pub tier: CapabilityTier,
    /// Optional tool **allowlist** (bare names, e.g. `get_balance`). When non-empty, ONLY these
    /// tools are permitted from this server: the wall denies any other tool under its
    /// `mcp__<name>__` prefix **fail-closed**. This is how a single endpoint that
    /// exposes its full surface to every token (cove serves all 37 tools on the read-only token)
    /// is held to a read-only subset under `cove-read`, with the trade tools reachable only via
    /// the `cove-trading` (settle-tier) sibling. Empty = expose everything the server provides.
    #[serde(default)]
    pub tools: Vec<String>,
    /// **Trust tier** of the data this server puts in play (the taint model — distinct from the
    /// capability `tier` above). Actually *calling* a tool from this server degrades the cycle's
    /// trust to (at most) this tier, capping how far the chain may then walk: e.g. `rootai`
    /// (3rd-party signals) `trust: public` → Express; `cove-read` (the operator's own wallet)
    /// `trust: self` → no degradation. Default `self`. A soul-inlined MCP isn't in this registry,
    /// so it taints **public** at access time (a soul can never self-grant trust).
    #[serde(default = "default_self_trust")]
    pub trust: TrustTier,
    /// **Minimum caller trust** to use this server — the AUTHORIZATION axis, distinct from
    /// `tier` (which STATE/class) and `trust` (the taint label of its DATA). The server is assembled
    /// for a cycle ONLY if the cycle's CURRENT (post-taint) trust ranks `>=` this tier in the lattice.
    /// `None` (default) = no trust gate (any cycle clearing the tier/state checks). Gating on the
    /// CURRENT trust means a high-trust cycle that touches `public` data **degrades and loses a
    /// `min_trust`-gated tool mid-cycle** (the firebreak, for free). e.g. a `telegram` send tool with
    /// `min_trust: org` is unreachable from a public tweet cycle, but reachable from a Telegram-org
    /// operator cycle — *unless* that cycle then reads a public link and degrades below `org`.
    #[serde(default)]
    pub min_trust: Option<TrustTier>,
    /// **Payload-scoped env** — `{ ENV_VAR: payload_field }`. At assembly the harness reads
    /// each `payload_field` from the waking stimulus and injects it into THIS server's env (next to the
    /// token), so the capability is locked to per-cycle data the MODEL never supplies. e.g. telegram
    /// `reply` is scoped `{ TELEGRAM_REPLY_CHAT: chat_id, TELEGRAM_REPLY_TO: message_id }` → it can only
    /// reply to the chat that woke this cycle; a prompt-injection can't redirect it to another chat
    /// (there is no `chat_id` argument). Absent payload field → that env var is simply not set.
    #[serde(default)]
    pub scope_env: std::collections::BTreeMap<String, String>,
    /// **Static env** injected into the server (stdio) — operator config the server needs that is
    /// neither a secret (`auth`) nor per-cycle (`scope_env`): e.g. the privileged `telegram-send`
    /// server's named-destination map (`{ TELEGRAM_DESTINATIONS: '{"operator":123,"trenchers":-100…}' }`).
    /// Keeping it here (the gitignored config) — not in the soul — means the duck can only reach
    /// operator-registered destinations, never an arbitrary/hallucinated chat_id.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

/// Which agent runtime drives the consciousness loop — the [`crate::runtime::RuntimeClient`] impl
/// and how it is spawned, with the engine's own config inline (tagged by `type`). The seam
///: v1 (Gitlawb/OpenClaude) vs a corp wiring differ only here + in construction.
/// **Only `openclaude` is wired**; `claude-code` (the Claude Code CLI/SDK directly, for corporate
/// prod) is the documented next variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RuntimeEngine {
    /// The bun bridge to the OpenClaude SDK.
    Openclaude {
        /// The `openclaude-bridge/` project dir (`bun run bridge.ts`).
        #[serde(default = "default_bridge_dir")]
        bridge_dir: String,
    },
}

impl Default for RuntimeEngine {
    fn default() -> Self {
        RuntimeEngine::Openclaude { bridge_dir: default_bridge_dir() }
    }
}

impl RuntimeEngine {
    /// The bridge project dir (openclaude). A no-op match today; the place a future engine slots in.
    pub fn bridge_dir(&self) -> &str {
        match self {
            RuntimeEngine::Openclaude { bridge_dir } => bridge_dir,
        }
    }
}

/// How the engine reaches models — the **model channel**, the provider endpoint, and (optionally,
/// inline in the gitignored config) the API key. Tagged by `type`. **Only `opengateway` is wired**;
/// `anthropic` (native catalog) parses but is a stub until a corp deployment needs it. The model
/// channel it selects is implemented by [`crate::runtime::openclaude::resolve_model_env`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Connector {
    /// OpenAI-compatible gateway (opengateway/gitlawb): the per-state model is routed via the
    /// child's `OPENAI_MODEL` env (the SDK rejects a gateway name passed as `options.model`).
    Opengateway {
        /// OpenAI-compat base URL → `OPENAI_BASE_URL` (e.g. `https://opengateway.gitlawb.com/v1`).
        /// `None` ⇒ taken from the forwarded env instead.
        #[serde(default)]
        api_url: Option<String>,
        /// Provider API key → `OPENAI_API_KEY`. `None` ⇒ from the forwarded env. (Inline only in the
        /// gitignored operator config; never in a committed file or the agent context.)
        #[serde(default)]
        api_key: Option<String>,
    },
    /// Native Anthropic catalog: the model is `options.model`. NOT YET WIRED (`cli::run` rejects it).
    Anthropic {
        #[serde(default)]
        api_key: Option<String>,
    },
}

impl Default for Connector {
    fn default() -> Self {
        Connector::Opengateway { api_url: None, api_key: None }
    }
}

impl Connector {
    /// Whether the per-invocation model is routed via the child's `OPENAI_MODEL` env (a gateway)
    /// rather than `options.model` (the Anthropic catalog). Feeds `resolve_model_env`.
    pub fn model_via_openai_env(&self) -> bool {
        matches!(self, Connector::Opengateway { .. })
    }

    /// The provider env (name → value) this connector contributes to the bridge — the base URL, the
    /// key, and any required flags. Applied OVER the forwarded env (so inline config wins). A `None`
    /// cred falls through to whatever the forwarded env supplies. The key never reaches agent context.
    pub fn env_overrides(&self) -> Vec<(String, String)> {
        match self {
            Connector::Opengateway { api_url, api_key } => {
                // The OpenAI-compat path REQUIRES this flag; the connector owns it (no env reliance).
                let mut env = vec![("CLAUDE_CODE_USE_OPENAI".to_string(), "1".to_string())];
                if let Some(u) = api_url {
                    env.push(("OPENAI_BASE_URL".to_string(), u.clone()));
                }
                if let Some(k) = api_key {
                    env.push(("OPENAI_API_KEY".to_string(), k.clone()));
                }
                env
            }
            Connector::Anthropic { api_key } => api_key
                .as_ref()
                .map(|k| vec![("ANTHROPIC_API_KEY".to_string(), k.clone())])
                .unwrap_or_default(),
        }
    }
}

/// The **runtime extensibility point** (the operator's choice of engine + connector + their config).
/// A single block so swapping to a different runtime/provider is one config edit, not a code change.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Which agent runtime drives the loop (only `openclaude` wired), with its own config inline.
    #[serde(default)]
    pub engine: RuntimeEngine,
    /// How it reaches models (only `opengateway` wired), with endpoint/key inline.
    #[serde(default)]
    pub connector: Connector,
    /// The DEFAULT model id; per-state overrides live in `tier_policy[state].model`. On the
    /// `opengateway` connector this seeds the per-invocation `OPENAI_MODEL`; on `anthropic` it is
    /// the `options.model` catalog name. `None` ⇒ whatever the runtime env already provides.
    #[serde(default)]
    pub model: Option<String>,
    /// Extra env var *names* forwarded into the runtime (values from the harness env, never the
    /// YAML). The connector's own creds (above) are applied on top. **Soul key never listed.**
    #[serde(default = "default_runtime_env")]
    pub env: Vec<String>,
    /// **OS isolation for delegated workers**. When enabled, an async worker
    /// ([`crate::harness`] `run_worker_detached`) runs its bridge inside a Docker container — the
    /// workspace bind-mounted rw, declared agent `volumes:` ro, everything else out of reach — so a
    /// worker's Bash/tools cannot touch the host. The duck itself is **never** containerized.
    #[serde(default)]
    pub worker_sandbox: WorkerSandboxConfig,
}

/// Docker isolation policy for delegated workers. Plain operator config; the CLI turns it
/// into an [`crate::sandbox::IsolationPolicy`] + [`crate::sandbox::DockerSandbox`]. The worker's
/// network is always **Full** (its bridge must egress to the model gateway — an egress-allowlist proxy
/// is future hardening, so `network` is not an operator knob that could fail-closed by mistake).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkerSandboxConfig {
    /// Run workers in Docker. `false` (default) ⇒ HostSandbox — today's behavior (the wall + tripwire
    /// still bound a worker logically, but Bash is not OS-confined).
    #[serde(default)]
    pub enabled: bool,
    /// The pre-built worker image (bun + the bridge + LINUX `node_modules`). Built at deploy time
    /// (`docker build -f Dockerfile.worker`), never by the harness at runtime.
    #[serde(default)]
    pub image: String,
    /// Container memory cap (e.g. `"512m"`). `None` ⇒ unbounded.
    #[serde(default)]
    pub memory: Option<String>,
    /// Container PID cap. `None` ⇒ unbounded.
    #[serde(default = "default_worker_pids")]
    pub pids_limit: Option<u32>,
    /// When Docker or the image is unavailable: `true` (default when enabled) ⇒ a `docker`-isolation
    /// worker HARD-FAILS (reports an error completion, never silently runs on the host — the safety
    /// claim holds). `false` ⇒ dev mode: warn + fall back to the host sandbox.
    #[serde(default = "default_true")]
    pub require: bool,
}

fn default_worker_pids() -> Option<u32> {
    Some(256)
}

/// **Dry-run** (testing/debug): when `enabled`, the WALL denies any tool whose name starts with a
/// `block` prefix — so the agent composes the action (visible in the runlog) but it never executes.
/// Tool-level (not class-level) so a no-real-trade run can still allow `simulate_swap`/`get_*` while
/// blocking `buy_token`. One mechanism for every MCP (twitter, cove, future) — no per-server env.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DryRunConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Tool-name PREFIXES the wall denies while `enabled` (e.g. `mcp__twitter__post`,
    /// `mcp__cove-trading__buy_token`, `mcp__cove-trading__create_`). Empty ⇒ nothing blocked.
    #[serde(default)]
    pub block: Vec<String>,
}

impl DryRunConfig {
    /// The block prefixes to enforce — the configured list when enabled, else empty (no blocking).
    pub fn active_block(&self) -> Vec<String> {
        if self.enabled {
            self.block.clone()
        } else {
            Vec::new()
        }
    }
}

/// `dack.config.yaml`. Hot-reloadable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DackConfig {
    /// The trusted operator DID — provenance check for `operator_signed`.
    pub operator_did: String,
    /// The operator-configured **trust lattice** (the taint/IFC model). Ranked low→high; each tier
    /// declares the max state it `reaches`. Omit to use the safe default
    /// (`public→express < self→reflect < operator_signed→reflect`).
    #[serde(default = "default_trust_tiers")]
    pub trust_tiers: Vec<TrustTierDef>,
    /// **Operator-signed sensor scripts** (the taint seed,): `sha256(source-hex) → trust`.
    /// A sensor whose source hashes to a listed entry seeds its cycle at that tier; an unsigned (or
    /// since-edited) script seeds `public`. This is operator code-review-as-signing — the agent
    /// authors a sensor, the operator reviews the EXACT bytes and signs the hash.
    #[serde(default)]
    pub signed_scripts: BTreeMap<String, TrustTier>,
    /// **Per-webhook trust**: a registered webhook path → the tier its payload is trusted
    /// at (e.g. an operator Telegram channel → `self`). An unlisted path seeds `public`.
    #[serde(default)]
    pub webhooks: BTreeMap<String, TrustTier>,
    /// **Reflect rate-limit**: the minimum seconds between self-modification (Reflect) runs,
    /// enforced by the harness clock for BOTH the scheduled Reflect and any transition-reached one —
    /// the rate-limit half of the guarantee (the taint ceiling is the other half). `0` disables.
    /// Default 1 day.
    #[serde(default = "default_reflect_interval")]
    pub reflect_min_interval_secs: i64,
    /// **Sticky-session idle TTL** (seconds): a resumable session unused for this long is dropped
    /// from the harness store (a fresh one starts next time). Bounds memory + stale thread context.
    /// Default 1 hour. `0` = never evict (not recommended).
    #[serde(default = "default_session_ttl")]
    pub session_ttl_secs: i64,
    /// **Sticky-session context-size cap** (tokens): after a resume whose context (input + cache-read
    /// tokens) exceeds this, the session is EVICTED so the next wake starts fresh — bounding cost and the
    /// bloat that drives confabulation (a fresh session reconstructs via the conversation runlog view).
    /// `None` (default) = size-eviction off (only the idle TTL applies). Set it in the operator config.
    #[serde(default)]
    pub session_max_context_tokens: Option<u64>,
    /// Operational logging (the `tracing` subscriber): level + format. Default `info`/`auto`.
    #[serde(default)]
    pub log: LogConfig,
    /// **Long-term memory** model. `memory/` is gated to this trust tier or above — a cycle below it may
    /// NOT write `memory/` (no laundered note from an untrusted in-run cycle; consolidation goes through
    /// org+ digest jobs / reflect). Tag-notes (short-term) share this floor. Default `org`.
    #[serde(default = "default_memory_write_min_trust")]
    pub memory_write_min_trust: String,
    /// Cap on the tag-notes catalogue (`runlogs/tag-notes.ndjson`): keep only the last N notes (the read
    /// tool dedups to latest-per-tag anyway). Default 200.
    #[serde(default = "default_tag_notes_max")]
    pub tag_notes_max: usize,
    /// Short-term memory retention: keep only the last N `runlogs/<date>.md` day-files (older deleted on
    /// boot + day-rollover) — bounds the repo and what the recall MCPs can scan. Default 10.
    #[serde(default = "default_runlog_retention_days")]
    pub runlog_retention_days: u64,
    /// **Load-shedding cap**: the max PENDING queue depth before the OLDEST `Low`-priority
    /// items are evicted (Normal+ is the protected floor, never shed; every eviction is logged).
    /// Protects a busy duck from a runaway low-priority fan-out backlog. `None` = unbounded.
    #[serde(default = "default_queue_max_depth")]
    pub queue_max_depth: Option<usize>,
    /// **Baton TTL** (seconds): a deferred baton continuation older than this is EXPIRED
    /// (dropped + logged) at dispatch instead of acting on a stale context — the world moved on while
    /// it waited behind higher-priority work. `None` (default) = a continuation never expires by age.
    #[serde(default)]
    pub baton_ttl_secs: Option<u64>,
    /// The state-prompt id harness-synthesized stimuli enter at (operator `dack say`, the
    /// boot back-online ping) — duties that carry no `entry:` of their own (). Defaults to
    /// the flat `perceive` prompt.
    #[serde(default = "default_entry_prompt")]
    pub default_entry: String,
    /// Per-consciousness-tier MCP policy — the operator's half of the
    /// capability handshake (which servers a state-prompt at each tier may import, and whether it
    /// may self-plug public read MCPs). An unconfigured tier defaults to locked + empty (nothing
    /// grantable). Keyed by `perceive`/`express`/`settle`/`reflect`.
    #[serde(default)]
    pub tier_policy: BTreeMap<ConsciousnessState, TierPolicy>,
    /// Names listed here are injected into the agent + sensor env. Reserved for
    /// recoverable/non-catastrophic values (API keys, handle, limits) —
    #[serde(default)]
    pub forwarded_env: Vec<String>,
    /// References only — values live in a separate secret store the YAML never inlines
    /// (so config is inspectable without exposing keys). The Soul DID key is here as a
    /// `file://` ref and is **never** forwarded to the agent.
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
    /// **Secrets providers** — trusted, harness-owned scripts that materialize a short-lived
    /// token env on demand (fetch + validity-check + rotate-only-when-needed). A duty/skill
    /// references a provider by `name` in its `secrets:` list; the harness runs the provider
    /// command and injects its JSON output. Adding a new secret (X, cove.trade, …) is a YAML
    /// entry + a script — **never a harness change** (`docs/secrets-and-sandbox.md`).
    #[serde(default)]
    pub secrets_providers: Vec<SecretsProviderConfig>,
    /// Cron schedule for the harness-entered Reflect run. `None` = manual
    /// (`dack reflect-now`) only.
    #[serde(default)]
    pub reflect_schedule: Option<String>,
    /// Path to the soul repo / actor bundle on the box (holds `stimuli/`, `SOUL.md`, …).
    #[serde(default = "default_soul_repo")]
    pub soul_repo: String,
    /// Embedded SQLite queue path (ephemeral;). Losing it loses only the queue.
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// Localhost bind for the webhook listener ( — nothing public without a proxy).
    #[serde(default = "default_webhook_addr")]
    pub webhook_addr: String,
    /// The **runtime** extensibility point — engine (which agent runtime) + connector (how it
    /// reaches models) + the engine wiring (bridge dir, default model, forwarded env). Folds the
    /// former top-level `bridge_dir`/`model`/`runtime_env`. Only `openclaude`+`opengateway` wired.
    #[serde(default)]
    pub runtime: RuntimeConfig,
    /// **Dry-run** (testing/debug): a wall-level tool-block list — the agent composes outward
    /// actions (visible in the runlog) but the wall denies their execution. Uniform across every MCP.
    #[serde(default)]
    pub dry_run: DryRunConfig,
    /// `gl` identity **directories** per role (each holds `identity.pem` + `ucan.json`). The
    /// Soul dir signs soul commits/pushes and **never enters agent env**.
    #[serde(default)]
    pub identities: IdentityDirs,
    /// The soul repo's signed push remote, e.g. `gitlawb://<soul-did>/dack-soul`. When set
    /// (with `identities.soul`), the harness uses the Gitlawb repo-host (signed `gitlawb://`
    /// push); otherwise plain-git degraded mode. `None` = local-only / offline.
    #[serde(default)]
    pub soul_remote: Option<String>,
    /// The gitlawb node the legacy single-`soul_remote` signed push targets (`GITLAWB_NODE`).
    /// Also the default node for any `soul_remotes` gitlawb entry that omits its own `node`.
    #[serde(default = "default_gitlawb_node")]
    pub gitlawb_node: String,
    /// **Soul push destinations** — a list of backends the harness pushes the soul repo to after
    /// each commit-sweep. The soul is ONE local git repo; each entry is a push *target*, so several
    /// remotes give redundancy: a reliable primary (GitHub / self-hosted) plus a best-effort
    /// decentralized mirror (`gitlawb://`). Empty ⇒ fall back to the legacy single `soul_remote`
    /// (and if that's unset too, local-only / no push). The backend is inferred from each URL's
    /// scheme unless `kind` overrides it.
    #[serde(default)]
    pub soul_remotes: Vec<SoulRemote>,
    /// **Runlog push destination** (optional) — the runlogs live in their OWN private git repo
    /// (`<soul>/runlogs/`, gitignored by the soul) so they never reach the PUBLIC soul (they carry chat
    /// detail + telegram handles). `None` ⇒ local-only (the repo stays on the box, disposable: a corrupt
    /// repo is wiped + re-init'd fresh). `Some` ⇒ also push there (e.g. a hosting-provided PRIVATE repo),
    /// which then becomes the durable backup a corrupt local repo is re-cloned from. Same spec as a
    /// `soul_remotes` entry (url/kind/node/identity/auth).
    #[serde(default)]
    pub runlog_remote: Option<SoulRemote>,
    /// Wall-clock budget (seconds) for one consciousness invocation, incl. the wall round-trips.
    /// A hung LLM/bridge elapses here rather than freezing the single-flight loop.
    #[serde(default = "default_invoke_timeout_secs")]
    pub invoke_timeout_secs: u64,
    /// Optional LONGER budget (seconds) for a **Reflect** invocation — the daily self-modification
    /// cycle, which may legitimately chew through prompts/skills/memory and deserves more than a chat
    /// reply's budget (e.g. 900 = 15 min). `None` ⇒ Reflect uses `invoke_timeout_secs` like everything
    /// else. Applies ONLY to Reflect; every other state keeps the default.
    #[serde(default)]
    pub reflect_invoke_timeout_secs: Option<u64>,
    /// Optional op-notify router URL (e.g. `http://127.0.0.1:8794/notify`). A TERMINAL dispatch
    /// failure — a cycle the harness gives up on — POSTs `{severity:"error", …}` there,
    /// fire-and-forget: the lane that reports failure must never become a failure mode itself.
    /// `None` = off. (The notify-lane primitive; connector routing lives in the router.)
    #[serde(default)]
    pub notify_url: Option<String>,
    /// MCP tool-name **prefixes** the wall treats as REVERSIBLE capabilities → `ToolClass::Post`
    /// (allowed in Express). The duck's act-phase tools, e.g. `mcp__twitter__` (post/reply).
    #[serde(default = "default_post_tools")]
    pub post_tools: Vec<String>,
    /// MCP tool-name **prefixes** the wall treats as IRREVERSIBLE authority → `ToolClass::SettleTx`
    /// (Settle only). MUST stay disjoint from `post_tools` (else a settle capability could classify
    /// as Post and run in Express). Settle is reachable only by an uncontaminated cycle (taint).
    #[serde(default = "default_settle_tools")]
    pub settle_tools: Vec<String>,
    /// **MCP capability registry** — operator-declared servers the duck can use in its
    /// act phases. Each server's `tier` derives the wall's classification (so declaring a server
    /// `tier: settle` makes its tools Settle-only); routes grant them via `capabilities:`. Adding
    /// a new tool (cove.trade, …) is an entry here + a token — never a harness/bridge code change.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// **Long-running side processes the harness owns** (the "modules" supervisor). Each is a
    /// persistent companion — e.g. the Telegram ingress adapter — that the harness spawns at boot,
    /// injects declared `secrets:` into, supervises (restart-on-exit with backoff), and tears down on
    /// shutdown. This is the contract for the future hosted-ducks orchestrator: a duck's full runtime
    /// (consciousness loop + its channels) is declared in ONE config and started by ONE process. A
    /// module is operator-trusted plumbing — NOT a consciousness seam (it touches no trust lattice);
    /// it only carries normalized events TO the harness webhook, where the trust contract applies.
    #[serde(default)]
    pub modules: Vec<ModuleConfig>,
}

/// One harness-supervised long-running side process (the "modules" supervisor). Declarative:
/// the harness spawns `command` in the repo root with `env` + the materialized `secrets:` env, and
/// keeps it alive (restart-on-exit, exponential backoff) until shutdown. Adding a channel/companion
/// is a config entry + its script — never a harness change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleConfig {
    /// Stable id for logs/diagnostics (e.g. `telegram-ingress`).
    pub name: String,
    /// argv — `command[0]` is the program (e.g. `["bun", "run", "openclaude-bridge/telegram-ingress.ts"]`).
    pub command: Vec<String>,
    /// Secrets-provider names whose materialized env is injected (e.g. `telegram_bot` →
    /// `TELEGRAM_BOT_TOKEN`). Resolved via the harness `SecretsBroker` at each (re)start, so a
    /// rotated token is picked up on the next restart — the module never holds a stale secret.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Static env injected into the module (non-secret config — paths, ids, flags).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory the module runs in. `None` ⇒ the harness process's cwd (where `dack run`
    /// was launched — the engine working tree that holds `openclaude-bridge/`, `secrets/`, the
    /// adapter's own config). Set it to pin a module to a different tree.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Whether the supervisor starts this module. `false` ⇒ declared but dormant (kept in config
    /// for documentation / one-flag enable). Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// One capability tool prefix the wall classifies by, plus its optional per-server tool allowlist.
/// `prefix` is `mcp__<server>__` (or an explicit `post_tools`/`settle_tools` entry); `tools` is the
/// server's bare-name allowlist (empty = all tools of that prefix are in-class).
#[derive(Debug, Clone)]
pub struct CapabilityPrefix {
    pub prefix: String,
    pub tools: Vec<String>,
}

impl CapabilityPrefix {
    /// A prefix with no tool allowlist (explicit `post_tools`/`settle_tools` entries, or a server
    /// that exposes its full surface).
    pub fn open(prefix: impl Into<String>) -> Self {
        Self { prefix: prefix.into(), tools: Vec::new() }
    }
}

/// The wall's capability tool-prefix classification, derived from the MCP registry
/// tiers merged with the explicit `post_tools`/`settle_tools`. Single source of truth.
#[derive(Debug, Clone, Default)]
pub struct CapabilityPrefixes {
    /// → `ToolClass::Read` (monitoring MCPs; safe everywhere).
    pub read: Vec<CapabilityPrefix>,
    /// → `ToolClass::Post` (reversible; Express).
    pub post: Vec<CapabilityPrefix>,
    /// → `ToolClass::SettleTx` (irreversible; Settle only).
    pub settle: Vec<CapabilityPrefix>,
}

/// `gl` identity directories per liability-boundary role. Each is a path to a dir
/// holding `identity.pem` (+ `ucan.json`). All optional: unset roles are simply absent (the
/// harness falls back where it can). The Soul key is harness-only — never forwarded.
/// Which backend signs and derives DIDs for the configured identity dirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityBackend {
    /// Native Ed25519 — reads the PKCS8 `identity.pem` directly, signs in-process. No external
    /// tools; keys are made with `dack keygen`. The default, and what makes a fresh deploy possible.
    #[default]
    Local,
    /// Legacy: shell out to the gitlawb `gl` CLI (`gl identity show`/`sign`). Requires the `gl`
    /// binary on PATH. Only for setups that already manage keys through gitlawb.
    Gitlawb,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityDirs {
    #[serde(default)]
    pub soul: Option<String>,
    #[serde(default)]
    pub operator: Option<String>,
    #[serde(default)]
    pub builder: Option<String>,
    /// Signing/DID backend (default `local`, native Ed25519). Set `gitlawb` to use the `gl` CLI.
    #[serde(default)]
    pub backend: IdentityBackend,
}

impl IdentityDirs {
    /// Resolve a role name (`"soul"`/`"operator"`/`"builder"`) to its identity dir, for a
    /// `soul_remotes` gitlawb entry that names which key signs the push (default: soul).
    pub fn dir(&self, role: &str) -> Option<&String> {
        match role {
            "soul" => self.soul.as_ref(),
            "operator" => self.operator.as_ref(),
            "builder" => self.builder.as_ref(),
            _ => None,
        }
    }
}

/// One push destination for the soul repo. The soul is a local git repo; this is *where* the
/// harness pushes it. The backend follows the URL scheme (`kind` overrides):
///   - `gitlawb://<did>/<repo>` → a signed ref-update via the `git-remote-gitlawb` helper.
///   - anything else (`git@host:…`, `https://…`, a local path) → plain `git push`.
///
/// A list of these is how a duck keeps its soul on a reliable primary AND a decentralized mirror
/// at once; a `required: false` mirror that's down is logged, never fatal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoulRemote {
    /// The push URL (its scheme selects the backend unless `kind` is set).
    pub url: String,
    /// Force the backend instead of inferring it from `url`'s scheme.
    #[serde(default)]
    pub kind: Option<RemoteKind>,
    /// gitlawb only: the node the signed push targets (`GITLAWB_NODE`). Defaults to `gitlawb_node`.
    #[serde(default)]
    pub node: Option<String>,
    /// gitlawb only: which `identities.<role>` key signs the push. Defaults to `"soul"`.
    #[serde(default)]
    pub identity: Option<String>,
    /// Plain-git over HTTPS only: an access token (e.g. a GitHub PAT) read from the daemon's
    /// environment and injected as an EPHEMERAL git credential at push time — never written to
    /// disk, never in agent env. SSH remotes (`git@…`) need none (the ambient key authenticates).
    #[serde(default)]
    pub auth: Option<RemoteAuth>,
    /// When true, a push failure to THIS target fails the cycle's push step. Default `false` ⇒
    /// best-effort: the failure is logged and the next cycle re-pushes (a flaky mirror never
    /// blocks the agent). Set `true` on the primary you require to durably persist the soul.
    #[serde(default)]
    pub required: bool,
}

/// The soul-push backend behind a [`SoulRemote`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteKind {
    /// Plain `git push` (GitHub / GitLab / Gitea / self-hosted / local path). No gitlawb signing.
    Git,
    /// Signed `gitlawb://` ref-update via the `git-remote-gitlawb` helper.
    Gitlawb,
}

/// HTTPS access-token auth for a plain-git [`SoulRemote`]. The token VALUE never lives in config:
/// `token_env` names a variable in the daemon's environment, read at push time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAuth {
    /// Env var (in the daemon's environment) holding the access token. Injected as the HTTPS
    /// password via an ephemeral credential helper — token stays out of argv, disk, and agent env.
    pub token_env: String,
    /// The basic-auth username. GitHub accepts any non-empty value for a PAT; defaults to the
    /// conventional `x-access-token`.
    #[serde(default = "default_git_user")]
    pub username: String,
}

fn default_git_user() -> String {
    "x-access-token".to_string()
}

fn default_entry_prompt() -> String {
    "perceive".to_string()
}
fn default_soul_repo() -> String {
    ".".to_string()
}
fn default_db_path() -> String {
    "dack.sqlite".to_string()
}
fn default_webhook_addr() -> String {
    "127.0.0.1:8787".to_string()
}
fn default_bridge_dir() -> String {
    "openclaude-bridge".to_string()
}
fn default_gitlawb_node() -> String {
    "https://node.gitlawb.com".to_string()
}
fn default_invoke_timeout_secs() -> u64 {
    300
}
fn default_post_tools() -> Vec<String> {
    vec!["mcp__twitter__".to_string()]
}
fn default_settle_tools() -> Vec<String> {
    vec!["mcp__bankr__".to_string(), "mcp__dac__".to_string()]
}
fn default_runtime_env() -> Vec<String> {
    vec![
        "OPENAI_API_KEY".to_string(),
        "OPENAI_BASE_URL".to_string(),
        "OPENAI_MODEL".to_string(),
    ]
}

impl DackConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::from_yaml(&text)
    }

    pub fn from_yaml(text: &str) -> Result<Self> {
        serde_yaml::from_str(text).map_err(DackError::from)
    }

    /// Look up an MCP capability server by name.
    pub fn mcp_server(&self, name: &str) -> Option<&McpServerConfig> {
        self.mcp_servers.iter().find(|s| s.name == name)
    }

    /// The resolved trust lattice (the taint model's order + state ceilings).
    pub fn lattice(&self) -> TrustLattice {
        TrustLattice { tiers: self.trust_tiers.clone() }
    }

    /// The trust seed for a sensor SCRIPT: the tier the operator signed its `sha256(source)`
    /// at, else `public` (an unsigned or since-edited script is untrusted code).
    pub fn script_trust(&self, source: &[u8]) -> TrustTier {
        self.signed_scripts
            .get(&sha256_hex(source))
            .cloned()
            .unwrap_or_else(TrustTier::public)
    }

    /// The trust seed for a webhook `path`: the operator-registered tier, else `public`.
    pub fn webhook_trust(&self, path: &str) -> TrustTier {
        self.webhooks.get(path).cloned().unwrap_or_else(TrustTier::public)
    }

    /// The trust label of a secrets provider by name (for the seed `meet`), or `self` if unknown.
    pub fn secret_trust(&self, name: &str) -> TrustTier {
        self.secrets_providers
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.trust.clone())
            .unwrap_or_else(TrustTier::self_)
    }

    /// The wall's capability prefix→class map: registry tiers (`mcp__<name>__`) merged
    /// with the explicit `post_tools`/`settle_tools`. The single source of truth the responder
    /// classifies from — so declaring a server `tier: settle` is what makes its tools Settle-only.
    pub fn capability_prefixes(&self) -> CapabilityPrefixes {
        let mut p = CapabilityPrefixes {
            read: Vec::new(),
            post: self.post_tools.iter().map(CapabilityPrefix::open).collect(),
            settle: self.settle_tools.iter().map(CapabilityPrefix::open).collect(),
        };
        for s in &self.mcp_servers {
            let cp = CapabilityPrefix {
                prefix: format!("mcp__{}__", s.name),
                tools: s.tools.clone(),
            };
            match s.tier {
                CapabilityTier::Read => p.read.push(cp),
                CapabilityTier::Post => p.post.push(cp),
                CapabilityTier::Settle => p.settle.push(cp),
            }
        }
        p
    }

    /// Startup safety check: a settle (irreversible) capability prefix must NEVER be a
    /// prefix-of / prefixed-by a reversible (post) or read prefix — else a settle tool could
    /// classify as `Post`/`Read` and run outside Settle. Checks the FULL derived map (registry +
    /// explicit lists). Called once at boot; a violation is a hard config error.
    pub fn validate_capabilities(&self) -> Result<()> {
        let p = self.capability_prefixes();
        let overlaps = |a: &str, b: &str| a.starts_with(b) || b.starts_with(a);
        for s in &p.settle {
            for other in p.post.iter().chain(p.read.iter()) {
                if overlaps(&s.prefix, &other.prefix) {
                    return Err(DackError::Config(format!(
                        "capability prefixes overlap: settle `{}` vs `{}` — must be disjoint \
                         (an irreversible tool must never classify as Post/Read and escape Settle)",
                        s.prefix, other.prefix
                    )));
                }
            }
        }
        Ok(())
    }

    /// The MCP [`TierPolicy`] for a consciousness `state` — the operator's half of the
    /// capability handshake. An unconfigured tier returns the safe default (locked, nothing
    /// grantable). Borrowed from the map when present, else a static default.
    pub fn tier_policy_for(&self, state: ConsciousnessState) -> std::borrow::Cow<'_, TierPolicy> {
        match self.tier_policy.get(&state) {
            Some(p) => std::borrow::Cow::Borrowed(p),
            None => std::borrow::Cow::Owned(TierPolicy::default()),
        }
    }

    /// The configured `gl` identity dirs keyed by role — fed to
    /// [`GitlawbIdentity::resolve`](crate::identity::gitlawb::GitlawbIdentity::resolve) so the
    /// harness can sign as the Soul (and, later, verify operator_signed via the Operator DID).
    pub fn identity_dirs(
        &self,
    ) -> std::collections::HashMap<crate::identity::IdentityRole, std::path::PathBuf> {
        use crate::identity::IdentityRole::*;
        let mut m = std::collections::HashMap::new();
        if let Some(d) = &self.identities.soul {
            m.insert(Soul, d.into());
        }
        if let Some(d) = &self.identities.operator {
            m.insert(Operator, d.into());
        }
        if let Some(d) = &self.identities.builder {
            m.insert(Builder, d.into());
        }
        m
    }

    /// The effective soul push destinations, normalizing the legacy single field into the list:
    /// the explicit `soul_remotes` if any, else a one-element list synthesized from `soul_remote`,
    /// else empty (local-only / no push). Backward-compatible: an old config keeps working.
    pub fn effective_soul_remotes(&self) -> Vec<SoulRemote> {
        if !self.soul_remotes.is_empty() {
            return self.soul_remotes.clone();
        }
        match &self.soul_remote {
            Some(url) => vec![SoulRemote {
                url: url.clone(),
                kind: None,
                node: None,
                identity: None,
                auth: None,
                required: false,
            }],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
operator_did: "did:key:z6MkOperator"
forwarded_env: [TWITTER_API_KEY, BANKR_API_KEY, BUILDER_DID_KEY, DACK_HANDLE, RATE_LIMIT]
secrets:
  soul_did_key: "file:///run/secrets/soul_did_key"
"#;

    #[test]
    fn adaptive_window_grows_each_time_half_the_budget_is_spent() {
        let a = AdaptiveCoalesce { initial_window_sec: 10, daily_credits: 100, max_window_sec: 1800 };
        assert_eq!(a.window_sec(0), 10, "fresh budget → initial window");
        assert_eq!(a.window_sec(49), 10, "under half spent → still initial");
        assert_eq!(a.window_sec(50), 100, "half spent → ×10");
        assert_eq!(a.window_sec(74), 100, "under three-quarters → still ×10");
        assert_eq!(a.window_sec(75), 1000, "three-quarters spent → ×100");
        assert_eq!(a.window_sec(88), 1800, "next step (×1000=10000) clamps to the cap");
        assert_eq!(a.window_sec(100), 1800, "budget exhausted → cap");
        assert_eq!(a.window_sec(250), 1800, "underwater (saturating) → cap, never panics");
    }

    #[test]
    fn parses_sample_config() {
        let cfg = DackConfig::from_yaml(SAMPLE).expect("config parses");
        assert_eq!(cfg.operator_did, "did:key:z6MkOperator");
        assert!(cfg.forwarded_env.contains(&"TWITTER_API_KEY".to_string()));
        // Soul key is a reference, never forwarded.
        assert!(cfg.secrets.contains_key("soul_did_key"));
        assert!(!cfg.forwarded_env.contains(&"SOUL_DID_KEY".to_string()));
    }

    #[test]
    fn runtime_defaults_to_openclaude_opengateway() {
        // A config with no `runtime:` block gets the wired default + the connector's model channel.
        let cfg = DackConfig::from_yaml(SAMPLE).unwrap();
        assert!(matches!(cfg.runtime.engine, RuntimeEngine::Openclaude { .. }));
        assert_eq!(cfg.runtime.engine.bridge_dir(), "openclaude-bridge");
        assert!(matches!(cfg.runtime.connector, Connector::Opengateway { .. }));
        assert!(cfg.runtime.model.is_none());
        // opengateway routes the model via the OPENAI_MODEL env channel.
        assert!(cfg.runtime.connector.model_via_openai_env());
    }

    #[test]
    fn parses_nested_runtime_block_with_inline_creds() {
        let cfg = DackConfig::from_yaml(
            "operator_did: \"did:x\"\n\
             runtime:\n\
             \x20 engine: { type: openclaude, bridge_dir: my-bridge }\n\
             \x20 connector: { type: opengateway, api_url: https://gw.example/v1, api_key: sk-test }\n\
             \x20 model: nvidia/nemotron-3-ultra-550b-a55b:free\n",
        )
        .unwrap();
        assert_eq!(cfg.runtime.engine.bridge_dir(), "my-bridge");
        assert_eq!(cfg.runtime.model.as_deref(), Some("nvidia/nemotron-3-ultra-550b-a55b:free"));
        assert!(cfg.runtime.connector.model_via_openai_env());
        // The inline creds become bridge env overrides (key reaches the bridge, never agent context).
        let overrides = cfg.runtime.connector.env_overrides();
        assert!(overrides.contains(&("OPENAI_BASE_URL".to_string(), "https://gw.example/v1".to_string())));
        assert!(overrides.contains(&("OPENAI_API_KEY".to_string(), "sk-test".to_string())));
        assert!(overrides.contains(&("CLAUDE_CODE_USE_OPENAI".to_string(), "1".to_string())));
    }

    #[test]
    fn parses_soul_remotes_list_with_backend_inference() {
        let cfg = DackConfig::from_yaml(
            "operator_did: \"did:x\"\n\
             soul_remotes:\n\
             \x20 - { url: \"git@github.com:obraztsov/dack-soul.git\", required: true }\n\
             \x20 - url: \"https://github.com/obraztsov/dack-soul.git\"\n\
             \x20   auth: { token_env: GITHUB_TOKEN }\n\
             \x20 - { url: \"gitlawb://did:key:zSoul/dack-soul\" }\n",
        )
        .unwrap();
        let rs = cfg.effective_soul_remotes();
        assert_eq!(rs.len(), 3);
        // First: SSH primary, required, no token, backend inferred git (non-gitlawb scheme).
        assert!(rs[0].required);
        assert!(rs[0].auth.is_none());
        assert!(!rs[0].url.starts_with("gitlawb://"));
        // Second: HTTPS with a token env + the default x-access-token username.
        assert_eq!(rs[1].auth.as_ref().unwrap().token_env, "GITHUB_TOKEN");
        assert_eq!(rs[1].auth.as_ref().unwrap().username, "x-access-token");
        assert!(!rs[1].required);
        // Third: a gitlawb mirror, best-effort by default.
        assert!(rs[2].url.starts_with("gitlawb://"));
        assert!(!rs[2].required);
    }

    #[test]
    fn legacy_single_soul_remote_synthesizes_a_one_element_list() {
        // An OLD config (single field) keeps working: it becomes a one-element best-effort list.
        let cfg = DackConfig::from_yaml(
            "operator_did: \"did:x\"\n\
             soul_remote: \"gitlawb://did:key:zSoul/dack-soul\"\n",
        )
        .unwrap();
        let rs = cfg.effective_soul_remotes();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].url, "gitlawb://did:key:zSoul/dack-soul");
        assert!(!rs[0].required, "legacy remote is best-effort (matches old behavior)");
        // No remote at all ⇒ empty list (local-only).
        let none = DackConfig::from_yaml("operator_did: \"did:x\"\n").unwrap();
        assert!(none.effective_soul_remotes().is_empty());
    }

    #[test]
    fn connector_channel_and_empty_creds_fall_through() {
        // opengateway → OPENAI_MODEL env channel; anthropic → options.model.
        assert!(Connector::Opengateway { api_url: None, api_key: None }.model_via_openai_env());
        assert!(!Connector::Anthropic { api_key: None }.model_via_openai_env());
        // With no inline creds, opengateway still sets the required flag but no base-URL/key (those
        // fall through to the forwarded env).
        let ov = Connector::Opengateway { api_url: None, api_key: None }.env_overrides();
        assert_eq!(ov, vec![("CLAUDE_CODE_USE_OPENAI".to_string(), "1".to_string())]);
    }

    #[test]
    fn validate_capabilities_rejects_overlapping_prefixes() {
        let mut cfg = DackConfig::from_yaml(SAMPLE).unwrap();
        // Defaults (mcp__twitter__ vs mcp__bankr__/mcp__dac__) are disjoint.
        assert!(cfg.validate_capabilities().is_ok());
        // A post prefix that swallows a settle prefix is rejected (settle would classify as Post).
        cfg.post_tools = vec!["mcp__".into()];
        cfg.settle_tools = vec!["mcp__bankr__".into()];
        assert!(cfg.validate_capabilities().is_err());
    }

    #[test]
    fn trust_lattice_ranks_meets_and_reaches() {
        let cfg = DackConfig::from_yaml(
            "operator_did: \"x\"\ntrust_tiers:\n  - { name: public, reaches: express }\n  \
             - { name: org, reaches: settle }\n  - { name: self, reaches: reflect }\n  \
             - { name: operator_signed, reaches: reflect }\n",
        )
        .unwrap();
        let l = cfg.lattice();
        // Rank ascends with the list; meet (taint) returns the lower-trust tier.
        assert!(l.rank(&TrustTier::public()) < l.rank(&TrustTier::from("org")));
        assert!(l.rank(&TrustTier::from("org")) < l.rank(&TrustTier::self_()));
        assert_eq!(l.meet(&TrustTier::self_(), &TrustTier::public()), TrustTier::public());
        assert_eq!(l.meet(&TrustTier::from("org"), &TrustTier::self_()), TrustTier::from("org"));
        // Each tier's state ceiling.
        assert_eq!(l.reaches(&TrustTier::public()), ConsciousnessState::Express);
        assert_eq!(l.reaches(&TrustTier::from("org")), ConsciousnessState::Settle);
        assert_eq!(l.reaches(&TrustTier::self_()), ConsciousnessState::Reflect);
        // AUTHORIZATION (`min_trust`): a cycle clears a requirement iff it ranks >= it.
        assert!(l.permits(&TrustTier::from("org"), &TrustTier::from("org")), "org clears min_trust:org");
        assert!(l.permits(&TrustTier::self_(), &TrustTier::from("org")), "a higher tier clears min_trust:org");
        assert!(!l.permits(&TrustTier::public(), &TrustTier::from("org")), "a lower tier does NOT clear min_trust:org");
        assert!(l.permits(&TrustTier::public(), &TrustTier::public()), "any cycle clears a public-min requirement");
        assert!(!l.permits(&TrustTier::from("bogus"), &TrustTier::from("org")), "an unknown cycle tier (rank 0) fails safe");
        // An unknown tier fails SAFE: rank 0 (lowest), reaches Express (never raises a ceiling).
        assert_eq!(l.rank(&TrustTier::from("bogus")), 0);
        assert_eq!(l.reaches(&TrustTier::from("bogus")), ConsciousnessState::Express);
        // The default lattice (no `trust_tiers:`) reproduces the pre-taint behavior.
        let d = DackConfig::from_yaml("operator_did: \"x\"").unwrap().lattice();
        assert_eq!(d.reaches(&TrustTier::self_()), ConsciousnessState::Reflect);
        assert_eq!(d.reaches(&TrustTier::public()), ConsciousnessState::Express);
    }

    #[test]
    fn seed_trust_from_signed_script_webhook_and_secret() {
        use sha2::{Digest, Sha256};
        let src = b"print('hi')\n";
        let hash: String = Sha256::digest(src).iter().map(|b| format!("{b:02x}")).collect();
        let yaml = format!(
            "operator_did: \"x\"\nsigned_scripts:\n  \"{hash}\": self\nwebhooks:\n  \"/telegram/op\": self\n\
             secrets_providers:\n  - {{ name: x, command: [echo], trust: public }}\n  - {{ name: cove, command: [echo], trust: self }}\n"
        );
        let cfg = DackConfig::from_yaml(&yaml).unwrap();
        // A SIGNED script's source hashes to its tier; an unsigned/edited script → public.
        assert_eq!(cfg.script_trust(src), TrustTier::self_());
        assert_eq!(cfg.script_trust(b"edited"), TrustTier::public());
        // A registered webhook path → its tier; an unknown path → public.
        assert_eq!(cfg.webhook_trust("/telegram/op"), TrustTier::self_());
        assert_eq!(cfg.webhook_trust("/random"), TrustTier::public());
        // Secret provider trust labels; an unknown provider → self (a no-op in the seed meet).
        assert_eq!(cfg.secret_trust("x"), TrustTier::public());
        assert_eq!(cfg.secret_trust("cove"), TrustTier::self_());
        assert_eq!(cfg.secret_trust("nope"), TrustTier::self_());
    }
}
