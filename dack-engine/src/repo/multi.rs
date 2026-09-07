//! Multi-remote soul repo-host. The soul bundle is ONE local git repo; this adapter holds that
//! single working tree (shared [`GitCore`], identical local semantics to [`PlainGitRepo`]) and a
//! list of independent *push targets* it fans out to after each commit-sweep.
//!
//! Why a list: a duck can keep its soul on a reliable primary (GitHub / self-hosted, plain `git
//! push`) AND a decentralized mirror (`gitlawb://`, signed ref-update) at the same time. Each
//! target is best-effort by default — a flaky/down mirror is logged and retried next cycle, never
//! blocking the agent — while a `required` target propagates its failure to the caller. The local
//! read/write/commit/status/revert path is untouched: only `push()` changed shape.
//!
//! [`PlainGitRepo`]: super::git::PlainGitRepo

use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use super::gitcore::GitCore;
use super::gitlawb::is_transient_push_error;
use super::{CommitId, CommitMeta, RepoChange, RepoHost, RepoPath};
use crate::error::{DackError, Result};

/// One resolved push destination. Built from a `SoulRemote` config entry; carries everything the
/// push needs so the target is self-contained (no config lookups at push time).
pub enum PushTarget {
    /// Plain `git push <url>`. `token` (if any) is an HTTPS credential injected ephemerally.
    Git {
        name: String,
        url: String,
        /// `(username, token_env)` for an HTTPS access-token push; `None` for SSH / local / public.
        token: Option<(String, String)>,
        required: bool,
    },
    /// Signed `gitlawb://` push: the `git-remote-gitlawb` helper signs the ref-update with the key
    /// at `key_path` (`GITLAWB_KEY`), pushing to `node` (`GITLAWB_NODE`).
    Gitlawb {
        name: String,
        url: String,
        key_path: PathBuf,
        node: String,
        required: bool,
    },
}

impl PushTarget {
    pub fn name(&self) -> &str {
        match self {
            PushTarget::Git { name, .. } | PushTarget::Gitlawb { name, .. } => name,
        }
    }

    pub fn required(&self) -> bool {
        match self {
            PushTarget::Git { required, .. } | PushTarget::Gitlawb { required, .. } => *required,
        }
    }

    /// Push the local repo's HEAD to this target, with bounded exponential-backoff retry on
    /// TRANSIENT failures (5xx / mid-stream disconnect / timeout — the young-node blips and any
    /// network flake). Permanent failures (auth, non-fast-forward) fail fast.
    async fn push(&self, core: &GitCore) -> Result<()> {
        const MAX_ATTEMPTS: u32 = 4;
        let mut attempt = 1u32;
        loop {
            let res = match self {
                PushTarget::Gitlawb {
                    url, key_path, node, ..
                } => {
                    let env = [
                        ("GITLAWB_KEY", key_path.to_string_lossy().into_owned()),
                        ("GITLAWB_NODE", node.clone()),
                    ];
                    core.push(url, &env).await
                }
                PushTarget::Git { url, token, .. } => match token {
                    None => core.push(url, &[]).await,
                    Some((user, token_env)) => match std::env::var(token_env) {
                        Ok(tok) => {
                            // Token rides in env (out of argv); the helper that reads it rides in a
                            // `-c credential.helper`. The leading empty helper clears inherited ones.
                            let env = [
                                ("DACK_GIT_USER", user.clone()),
                                ("DACK_GIT_TOKEN", tok),
                            ];
                            let cfg = [
                                "credential.helper=".to_string(),
                                "credential.helper=!f() { echo username=\"$DACK_GIT_USER\"; \
                                 echo password=\"$DACK_GIT_TOKEN\"; }; f"
                                    .to_string(),
                            ];
                            core.push_with(url, &env, &cfg).await
                        }
                        Err(_) => Err(DackError::Repo(format!(
                            "soul remote `{}`: token env `{token_env}` not set in the daemon \
                             environment",
                            self.name()
                        ))),
                    },
                },
            };
            match res {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if attempt >= MAX_ATTEMPTS || !is_transient_push_error(&e.to_string()) {
                        return Err(e);
                    }
                    let backoff = std::time::Duration::from_millis(500u64 << (attempt - 1));
                    tracing::info!(
                        "soul push `{}` attempt {attempt}/{MAX_ATTEMPTS} failed (transient): {e} \
                         — retrying in {backoff:?}",
                        self.name()
                    );
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                }
            }
        }
    }
}

/// A soul repo-host that pushes to many targets. Local ops delegate to the single [`GitCore`];
/// `push()` fans out (best-effort per target unless `required`).
pub struct MultiRemoteRepo {
    core: GitCore,
    targets: Vec<PushTarget>,
    /// Unpushed-commits flag: set by any mutating op, cleared by a successful `push()`. Lets `push()`
    /// skip the per-cycle network round-trip when nothing changed (so a chat-only cycle that touches no
    /// soul file doesn't push at all). Starts `true` so the first push after boot flushes any commits a
    /// crash left local. A `required`-target failure keeps it set (retry next cycle); a best-effort
    /// failure does NOT (so a permanently-down mirror can't force a push every cycle).
    dirty: AtomicBool,
}

impl MultiRemoteRepo {
    pub fn new(
        workdir: impl Into<PathBuf>,
        author_did: impl Into<String>,
        targets: Vec<PushTarget>,
    ) -> Self {
        Self {
            core: GitCore::new(workdir, author_did),
            targets,
            dirty: AtomicBool::new(true),
        }
    }

    pub fn target_names(&self) -> Vec<&str> {
        self.targets.iter().map(|t| t.name()).collect()
    }
}

#[async_trait]
impl RepoHost for MultiRemoteRepo {
    async fn read_file(&self, path: &RepoPath) -> Result<Vec<u8>> {
        self.core.read_file(path).await
    }
    async fn write_file(
        &self,
        path: &RepoPath,
        contents: &[u8],
        commit: &CommitMeta,
    ) -> Result<CommitId> {
        let r = self.core.write_file(path, contents, commit).await;
        if r.is_ok() {
            self.dirty.store(true, Ordering::Relaxed);
        }
        r
    }
    async fn list_dir(&self, path: &RepoPath, max_depth: usize) -> Result<Vec<RepoPath>> {
        self.core.list_dir(path, max_depth).await
    }
    async fn revert_file(&self, path: &RepoPath) -> Result<CommitId> {
        let r = self.core.revert_file(path).await;
        if r.is_ok() {
            self.dirty.store(true, Ordering::Relaxed);
        }
        r
    }
    async fn remove_file(&self, path: &RepoPath, commit: &CommitMeta) -> Result<CommitId> {
        let r = self.core.remove(path, &commit.message, &commit.author_did).await;
        if r.is_ok() {
            self.dirty.store(true, Ordering::Relaxed);
        }
        r
    }
    async fn status(&self) -> Result<Vec<RepoChange>> {
        self.core.status_porcelain().await
    }
    async fn commit_paths(
        &self,
        paths: &[RepoPath],
        commit: &CommitMeta,
    ) -> Result<Option<CommitId>> {
        let r = self
            .core
            .commit_paths(paths, &commit.message, &commit.author_did)
            .await;
        if matches!(r, Ok(Some(_))) {
            self.dirty.store(true, Ordering::Relaxed);
        }
        r
    }
    async fn restore_to_head(&self, change: &RepoChange) -> Result<()> {
        self.core.restore_to_head(change).await
    }

    /// Fan out to every target. A `required` target's failure is the returned error (the first
    /// one); a best-effort target's failure is logged and swallowed. No targets ⇒ no-op (local
    /// commits already happened).
    async fn push(&self) -> Result<()> {
        // Skip the network round-trip when nothing was committed since the last successful push — a
        // chat-only cycle (no soul file touched; runlogs live in their own repo now) pushes nothing.
        if !self.dirty.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut first_required_err: Option<DackError> = None;
        for t in &self.targets {
            match t.push(&self.core).await {
                Ok(()) => tracing::info!("soul pushed → {}", t.name()),
                Err(e) => {
                    if t.required() {
                        tracing::error!("soul push FAILED (required) → {}: {e}", t.name());
                        if first_required_err.is_none() {
                            first_required_err = Some(e);
                        }
                    } else {
                        tracing::warn!(
                            "soul push failed (best-effort, re-pushed next cycle) → {}: {e}",
                            t.name()
                        );
                    }
                }
            }
        }
        match first_required_err {
            // A required target failed → stay dirty so the next cycle retries. A best-effort-only
            // failure still clears dirty (a permanently-down mirror must not force a push every cycle).
            Some(e) => Err(e),
            None => {
                self.dirty.store(false, Ordering::Relaxed);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    async fn bare(path: &PathBuf) {
        Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(path)
            .output()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fans_out_to_multiple_remotes_and_tolerates_a_down_best_effort_one() {
        let base = std::env::temp_dir().join(format!("dack-multi-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&base).await;
        let work = base.join("work");
        let r1 = base.join("primary.git");
        let r2 = base.join("mirror.git");
        tokio::fs::create_dir_all(&work).await.unwrap();
        bare(&r1).await;
        bare(&r2).await;
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.name", "s"],
            vec!["config", "user.email", "s@d"],
        ] {
            Command::new("git").arg("-C").arg(&work).args(&args).output().await.unwrap();
        }

        let targets = vec![
            PushTarget::Git {
                name: "primary".into(),
                url: r1.to_string_lossy().into_owned(),
                token: None,
                required: true,
            },
            PushTarget::Git {
                name: "mirror".into(),
                url: r2.to_string_lossy().into_owned(),
                token: None,
                required: false,
            },
            // A best-effort target pointing at a path that isn't a repo → fails, must NOT abort.
            PushTarget::Git {
                name: "dead-mirror".into(),
                url: base.join("does-not-exist.git").to_string_lossy().into_owned(),
                token: None,
                required: false,
            },
        ];
        let repo = MultiRemoteRepo::new(&work, "did:key:zSoul", targets);
        repo.write_file(
            &RepoPath("SOUL.md".into()),
            b"I am.\n",
            &CommitMeta { message: "genesis".into(), author_did: "did:key:zSoul".into() },
        )
        .await
        .unwrap();

        // Both reachable remotes get the ref; the dead best-effort one is swallowed → Ok.
        repo.push().await.unwrap();
        for r in [&r1, &r2] {
            let got = Command::new("git")
                .arg("-C")
                .arg(r)
                .args(["rev-parse", "refs/heads/main"])
                .output()
                .await
                .unwrap();
            assert!(got.status.success(), "remote {r:?} should have the branch");
        }

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn a_required_target_failure_propagates() {
        let base = std::env::temp_dir().join(format!("dack-multi-req-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&base).await;
        let work = base.join("work");
        tokio::fs::create_dir_all(&work).await.unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.name", "s"],
            vec!["config", "user.email", "s@d"],
        ] {
            Command::new("git").arg("-C").arg(&work).args(&args).output().await.unwrap();
        }
        let targets = vec![PushTarget::Git {
            name: "primary".into(),
            url: base.join("nope.git").to_string_lossy().into_owned(),
            token: None,
            required: true,
        }];
        let repo = MultiRemoteRepo::new(&work, "did:key:zSoul", targets);
        repo.write_file(
            &RepoPath("SOUL.md".into()),
            b"x\n",
            &CommitMeta { message: "g".into(), author_did: "did:key:zSoul".into() },
        )
        .await
        .unwrap();
        assert!(repo.push().await.is_err(), "a required target failure must surface");

        let _ = tokio::fs::remove_dir_all(&base).await;
    }
}
