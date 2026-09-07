//! Operational logging — the `tracing` subscriber init. ONE place sets up leveled, structured,
//! span-aware logs to **stdout** (so `docker logs` on a hosted duck container captures them, and an
//! operator backend can pull them). This is the operational layer; the **runlog** (the agent's
//! soul-committed self-narrative, `src/runlog`) is a separate thing and untouched.
//!
//! Levels (operator intent): `trace` = everything incl. context-assembly internals / raw payloads;
//! `debug` = scheduling + decisions; `info` = lifecycle (boot, cycles, outward actions, evictions);
//! `warn`/`error` = fallbacks + failures. Per-cycle **spans** (`cycle{stim=…}`) correlate interleaved
//! lines once the scheduler goes multi-threaded.

use std::io::IsTerminal;

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::config::LogConfig;

/// Initialise the global subscriber from config. Idempotent: a second call (e.g. an operator command
/// after `run`, or a test) is a no-op rather than a panic. `RUST_LOG` overrides `cfg.level` for
/// per-module control (e.g. `RUST_LOG=dack::harness=debug,info`). Format `auto` picks pretty TEXT on a
/// TTY and JSON otherwise (containerized/piped → machine-parseable). Logs go to **stdout**.
pub fn init(cfg: &LogConfig) {
    // `cfg.level` is the BASE; RUST_LOG (if present) takes precedence for fine-grained control.
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cfg.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let json = match cfg.format.as_str() {
        "json" => true,
        "text" => false,
        _ => !std::io::stdout().is_terminal(), // auto: JSON when not a TTY (docker/piped)
    };

    let registry = tracing_subscriber::registry().with(filter);
    if json {
        // Flattened JSON lines (one object per event) with the FULL span stack attached (the `spans`
        // array carries the parent `cycle{stim=…}` on every line — the correlation key for concurrent
        // cycles once the scheduler is multi-threaded).
        let _ = registry
            .with(fmt::layer().json().with_current_span(true).with_span_list(true).with_writer(std::io::stdout))
            .try_init();
    } else {
        // Human-friendly for local dev: compact, span context shown.
        let _ = registry
            .with(fmt::layer().with_target(true).with_writer(std::io::stdout))
            .try_init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent_and_does_not_panic() {
        // Both calls + both formats must be safe (try_init swallows the "already set" error).
        init(&LogConfig { level: "debug".into(), format: "text".into() });
        init(&LogConfig { level: "info".into(), format: "json".into() });
    }

    #[test]
    fn log_config_defaults_are_info_auto() {
        let c = LogConfig::default();
        assert_eq!((c.level.as_str(), c.format.as_str()), ("info", "auto"));
    }
}
