//! # DACK actor-scheduler harness
//!
//! A self-sovereign, event-driven DAC **actor**: an agent that sleeps until a stimulus
//! wakes it, reasons in one of four bounded consciousness states, acts within
//! guardrails, updates its own memory and future wake conditions, and sleeps again.
//!
//! This crate is the **harness** (the actor-scheduler) — *not* the model. The model
//! (consciousness substrate) is rented from OpenClaude over **stdio** — a child Node bridge
//! running its SDK ([`runtime`]); the harness is the dumb, deterministic subconscious around it.
//!
//! ## The load-bearing invariants — preserve these
//! 1. **Intelligence lives in exactly one layer.** Sources and the bus are dumb; only
//!    the consciousness states ([`state`]) run a model. Never put an LLM in the plumbing.
//! 2. **Filters on cognition: no. Boundaries on irreversibility: yes.** The irreversibility
//!    predicate ([`runtime::settle`], unwired in v1) sits only on the one irreversible doorway;
//!    cognition is sovereign. The per-state *tool* wall is [`runtime::action_required`] (next).
//! 3. **The protection cannot live in the layer under attack.** The wall is the
//!    out-of-process [`runtime::action_required`] responder, in config the agent can't write.
//! 4. **Partition by reversibility** — the cut behind the four states.
//! 5. **Provenance, not rhetoric, establishes trust** ([`identity`], trust tiers).
//!
//! ## Module map (the spine → the seams → the orchestrator)
//! - [`model`]    — Stimulus, Baton, AgentOutput, RunLogEntry (the four data objects)
//! - [`state`]    — the four consciousness states + write-gating + transition rules
//! - [`config`]   — operator control plane / YAML
//! - [`stimuli`]  — the job-description registry (`STIMULUS.md` format)
//! - [`sensor`]   — the pure-perceiver sensor contract
//! - [`bus`] / [`queue`] / [`sources`] / [`webserver`] — the dumb stimulus pipeline
//! - [`runtime`]  — the OpenClaude stdio-bridge seam + the `action_required` wall (taint-gated Settle)
//! - [`identity`] / [`repo`] — the swappable seams (Gitlawb v1; corp later). Memory has **no**
//!   Rust seam — the agent reaches it through the path-gated file tools.
//! - [`secrets`] / [`runlog`] — harness-owned plumbing stores
//! - [`harness`]  — the actor-scheduler that wires it all together

pub mod bus;
pub mod cli;
pub mod config;
pub mod agent_def;
pub mod error;
pub mod harness;
pub mod health;
pub mod identity;
pub mod logging;
pub mod model;
pub mod queue;
pub mod repo;
pub mod runlog;
pub mod runtime;
pub mod sandbox;
pub mod secrets;
pub mod sensor;
pub mod sources;
pub mod state;
pub mod state_prompt;
pub mod stimuli;
pub mod webserver;

pub use error::{DackError, Result};
