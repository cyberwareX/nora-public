//! Crate-wide error type. One enum, `?`-friendly across every module so the
//! harness can bubble failures into a RunLog entry (logging-not-rollback)
//! rather than crashing the single-flight loop.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DackError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// Malformed config / control plane.
    #[error("config: {0}")]
    Config(String),

    /// A sensor violated the contract or exited non-zero.
    #[error("sensor: {0}")]
    Sensor(String),

    /// The OpenClaude gRPC runtime seam.
    #[error("runtime: {0}")]
    Runtime(String),

    /// The model invocation exceeded its wall-clock budget (the LLM/bridge hung with no completion).
    /// Kept DISTINCT from `Runtime` so the dispatch loop can RETRY a zero-completion hang (bounded) —
    /// a hung bridge that did nothing is safe to re-attempt, where a generic runtime error is terminal.
    #[error("timeout: {0}")]
    Timeout(String),

    /// Repo-host adapter (Gitlawb / plain-git fallback).
    #[error("repo: {0}")]
    Repo(String),

    /// Identity-provider adapter (DID signing).
    #[error("identity: {0}")]
    Identity(String),

    /// A stimulus definition under `stimuli/` could not be parsed/registered.
    #[error("stimulus: {0}")]
    Stimulus(String),

    /// The embedded SQLite queue / durable store.
    #[error("queue: {0}")]
    Queue(String),

    /// The `action_required` responder rejected a tool call — this is
    /// a *normal* outcome (the wall doing its job), surfaced as an error only when
    /// a caller treated a denial as fatal.
    #[error("denied by responder: {0}")]
    Denied(String),

    #[error("not implemented (scaffold): {0}")]
    NotImplemented(&'static str),
}

impl DackError {
    /// True for a model-invocation timeout (a hung bridge). The one error class the dispatch loop
    /// retries (bounded) — a zero-completion hang is safe to re-attempt.
    pub fn is_timeout(&self) -> bool {
        matches!(self, DackError::Timeout(_))
    }
}

pub type Result<T> = std::result::Result<T, DackError>;
