//! Repo-host seam. The core speaks **"commit to repo Y"**; the
//! actor bundle is *a git repo*, so the degraded-mode fallback is a plain git remote
//! with no state-logic change ( — Gitlawb node software is pre-alpha).
//!
//! This is where the soul/memory/skills/runlogs live (off-VPS): durable state
//! with a different lifetime than the ephemeral SQLite queue.

use async_trait::async_trait;

use crate::error::Result;

/// A path within the actor bundle, e.g. `memory/log.md`, `skills/twitter/SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoPath(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitId(pub String);

/// One entry of `git status --porcelain` — a working-tree path that differs from HEAD.
/// Feeds the per-run commit-sweep (commit the expected delta) and the soul-integrity
/// tripwire (revert anything outside the running state's `writable_dirs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoChange {
    /// The two-char porcelain code, e.g. `"??"` (untracked), `" M"` (modified), `"A "` (added).
    pub code: String,
    pub path: RepoPath,
}

impl RepoChange {
    /// An untracked path (`??`) — restored by deletion, not `checkout` (it has no HEAD version).
    pub fn is_untracked(&self) -> bool {
        self.code == "??"
    }
}

/// Who authored a commit, and why. v1: all soul-repo commits are authored by the
/// harness on the duck's behalf, signed with the Soul DID.
#[derive(Debug, Clone)]
pub struct CommitMeta {
    pub message: String,
    /// The DID this commit is attributed to (Soul for soul-repo writes).
    pub author_did: String,
}

#[async_trait]
pub trait RepoHost: Send + Sync {
    async fn read_file(&self, path: &RepoPath) -> Result<Vec<u8>>;

    /// Write + commit in one ministerial step. The harness signs; the agent's tool
    /// call to write only *reaches* here after the `action_required` responder has
    /// confirmed the target dir is in the current state's `writable_dirs`.
    async fn write_file(&self, path: &RepoPath, contents: &[u8], commit: &CommitMeta)
        -> Result<CommitId>;

    /// List entries under `path` to `max_depth` — used by the `stimuli/` walker
    /// (depth ≤ 2) and by memory `ls`.
    async fn list_dir(&self, path: &RepoPath, max_depth: usize) -> Result<Vec<RepoPath>>;

    /// Revert a single file to its prior commit — the ONE rollback case:
    /// a run that fails to start because a skill/stimulus file is malformed.
    async fn revert_file(&self, path: &RepoPath) -> Result<CommitId>;

    /// Delete a tracked file and commit the removal (harness-authored). Used by runlog retention to
    /// age off old day-files. Default: unsupported (only the git-backed repos used for runlogs override).
    async fn remove_file(&self, _path: &RepoPath, _commit: &CommitMeta) -> Result<CommitId> {
        Err(crate::error::DackError::NotImplemented("remove_file"))
    }

    /// The soul working-tree delta vs HEAD (untracked included) — what a run *actually*
    /// changed, for the post-run sweep + tripwire. Empty = clean.
    async fn status(&self) -> Result<Vec<RepoChange>>;

    /// Stage + commit exactly the given paths (the run's expected, allowlisted delta),
    /// attributed to `commit.author_did` (the Soul DID). `Ok(None)` when nothing changed —
    /// never an empty commit. The interlock that makes tool-driven `Write`/`Edit` durable.
    async fn commit_paths(&self, paths: &[RepoPath], commit: &CommitMeta) -> Result<Option<CommitId>>;

    /// Discard a working-tree change, restoring `path` to its HEAD version (or deleting it
    /// if it was untracked) — the tripwire's revert for an out-of-allowlist write. Does NOT
    /// commit (the revert is the absence of a change, not a new commit).
    async fn restore_to_head(&self, change: &RepoChange) -> Result<()>;

    /// Push committed history to the configured remote — plain `git push` for [`git::PlainGitRepo`],
    /// a signed `gitlawb://` ref-update for [`gitlawb::GitlawbRepo`]. No-op when no remote is set
    /// (offline / degraded mode). Called at the end of a successful commit-sweep.
    async fn push(&self) -> Result<()>;
}

pub mod git;
pub mod gitcore;
pub mod gitlawb;
pub mod multi;
