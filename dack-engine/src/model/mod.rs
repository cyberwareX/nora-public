//! The domain model — the *spine* of the harness.
//!
//! Four first-class data objects, each owned by a different writer-of-record
//!:
//!   - [`stimulus::Stimulus`] — inert event data, harness-owned, never agent-written.
//!   - [`baton::Baton`]       — the in-flight handoff between consciousness states;
//!                              the firebreak made structural.
//!   - [`proposal::AgentOutput`] — the agent's ONLY return channel.
//!   - [`runlog::RunLogEntry`]   — the harness-authored record of what happened.

pub mod baton;
pub mod proposal;
pub mod runlog;
pub mod stimulus;
