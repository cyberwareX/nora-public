//! Gitlawb repo-host adapter — v1 default.
//!
//! Grounded against `gl` 0.3.8: the *same* local git operations as [`PlainGitRepo`]
//! (shared [`GitCore`]), differing only in (a) the remote is a `gitlawb://` URL served by
//! the `git-remote-gitlawb` helper, and (b) `gl repo create` registers the repo on a node
//! first. The helper signs the ref-update with the identity in the configured `gl` dir on
//! push — so the Soul DID's key signs pushes without ever entering agent env.
//!
//! Local ops (read/write/commit/list/revert) work offline; push needs a reachable node
//! (`--node`, default `https://node.gitlawb.com`). Setup commands are in.

use async_trait::async_trait;
use std::path::PathBuf;

use super::gitcore::GitCore;
use super::{CommitId, CommitMeta, RepoChange, RepoHost, RepoPath};
use crate::error::Result;

pub struct GitlawbRepo {
    core: GitCore,
    /// e.g. `gitlawb://<soul-did>/dack-soul` — the push remote served by git-remote-gitlawb.
    pub remote: String,
    /// The `gl` identity dir whose key signs pushes (the Soul dir; harness-only). The helper
    /// reads `GITLAWB_KEY=<identity_dir>/identity.pem` and the `ucan.json` beside it.
    pub identity_dir: PathBuf,
    /// The gitlawb node the helper pushes to (`GITLAWB_NODE`), e.g. `https://node.gitlawb.com`.
    pub node: String,
}

impl GitlawbRepo {
    pub fn new(
        workdir: impl Into<PathBuf>,
        author_did: impl Into<String>,
        remote: impl Into<String>,
        identity_dir: impl Into<PathBuf>,
        node: impl Into<String>,
    ) -> Self {
        Self {
            core: GitCore::new(workdir, author_did),
            remote: remote.into(),
            identity_dir: identity_dir.into(),
            node: node.into(),
        }
    }
}

#[async_trait]
impl RepoHost for GitlawbRepo {
    async fn read_file(&self, path: &RepoPath) -> Result<Vec<u8>> {
        self.core.read_file(path).await
    }
    async fn write_file(
        &self,
        path: &RepoPath,
        contents: &[u8],
        commit: &CommitMeta,
    ) -> Result<CommitId> {
        // Local commit is identical to plain git; the gitlawb:// push (signed by the helper
        // using `identity_dir`) is layered on in finish once a node is configured.
        self.core.write_file(path, contents, commit).await
    }
    async fn list_dir(&self, path: &RepoPath, max_depth: usize) -> Result<Vec<RepoPath>> {
        self.core.list_dir(path, max_depth).await
    }
    async fn revert_file(&self, path: &RepoPath) -> Result<CommitId> {
        self.core.revert_file(path).await
    }
    async fn status(&self) -> Result<Vec<RepoChange>> {
        self.core.status_porcelain().await
    }
    async fn commit_paths(
        &self,
        paths: &[RepoPath],
        commit: &CommitMeta,
    ) -> Result<Option<CommitId>> {
        self.core
            .commit_paths(paths, &commit.message, &commit.author_did)
            .await
    }
    async fn restore_to_head(&self, change: &RepoChange) -> Result<()> {
        self.core.restore_to_head(change).await
    }
    /// Signed `gitlawb://` push: the `git-remote-gitlawb` helper signs the ref-update with the
    /// Soul key at `GITLAWB_KEY` (never in agent env), pushing to `GITLAWB_NODE`. The Soul-DID
    /// commit *author* is attribution; THIS signature is the cryptographic provenance.
    ///
    /// **Bounded retry with exponential backoff.** The node is young and occasionally flaky (5xx,
    /// mid-stream disconnects, timeouts); a TRANSIENT failure gets a few quick retries here before
    /// the cycle's coarse fallback (`push_soul` keeps the commits local and re-pushes next cycle).
    /// Permanent failures (auth, non-fast-forward) fail fast — retrying can't fix them.
    async fn push(&self) -> Result<()> {
        let key = self.identity_dir.join("identity.pem");
        let env = [
            ("GITLAWB_KEY", key.to_string_lossy().into_owned()),
            ("GITLAWB_NODE", self.node.clone()),
        ];
        // 4 attempts, backoff 0.5s → 1s → 2s (≈3.5s worst case). Tunable; the loop is single-flight.
        const MAX_ATTEMPTS: u32 = 4;
        let mut attempt = 1u32;
        loop {
            match self.core.push(&self.remote, &env).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if attempt >= MAX_ATTEMPTS || !is_transient_push_error(&e.to_string()) {
                        return Err(e);
                    }
                    let backoff = std::time::Duration::from_millis(500u64 << (attempt - 1));
                    tracing::warn!(
                        "gitlawb push attempt {attempt}/{MAX_ATTEMPTS} failed (transient): {e} — retrying in {backoff:?}"
                    );
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                }
            }
        }
    }
}

/// Whether a push failure looks like a TRANSIENT node/network blip (worth a retry) rather than a
/// permanent rejection (auth, conflict). The gitlawb node returns plain git/HTTP errors, so we
/// classify on the message — conservative: only clear transient signatures retry; everything else
/// (incl. `403`/`non-fast-forward`) fails fast so we don't loop on an unfixable error.
pub(crate) fn is_transient_push_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "500", "502", "503", "504",
        "internal server error", "bad gateway", "service unavailable", "gateway timeout",
        "unexpected disconnect", "hung up", "sideband", "early eof", "rpc failed",
        "timed out", "timeout", "connection reset", "connection refused", "broken pipe",
        "could not read from remote", "failed to connect", "temporarily unavailable",
    ]
    .iter()
    .any(|p| m.contains(p))
}

#[cfg(test)]
mod tests {
    use super::is_transient_push_error;

    #[test]
    fn classifies_transient_vs_permanent_push_errors() {
        // The exact failure seen against the young node → retry.
        assert!(is_transient_push_error(
            "repo: git push gitlawb://…/dack-soul HEAD:refs/heads/main: Error: POST \
             /git-receive-pack returned 500 Internal Server Error\nsend-pack: unexpected \
             disconnect while reading sideband packet\nfatal: the remote end hung up unexpectedly"
        ));
        assert!(is_transient_push_error("fatal: unable to access: Connection reset by peer"));
        assert!(is_transient_push_error("error: RPC failed; HTTP 503 Service Unavailable"));
        // Permanent rejections → fail fast (retrying can't fix them).
        assert!(!is_transient_push_error("remote: 403 Forbidden — not authorized to push"));
        assert!(!is_transient_push_error(
            "! [rejected] main -> main (non-fast-forward)\nUpdates were rejected"
        ));
        assert!(!is_transient_push_error("fatal: could not read Username: terminal prompts disabled"));
    }
}
