//! Shared git operations behind the [`RepoHost`](super::RepoHost) seam.
//!
//! Grounded against `gl` 0.3.8: Gitlawb has **no `repo commit`/`push`** — commits and
//! pushes go through plain `git`, and the `git-remote-gitlawb` helper signs the ref-update
//! on a `gitlawb://` remote. So `GitlawbRepo` and `PlainGitRepo` share *this* core and
//! differ only in the remote URL + push-signing identity (made literal: the
//! bundle is just a git repo).
//!
//! The duck "authors its own commits" via per-commit `git -c user.name=<DID>` — the commit
//! history is attributed to the Soul DID (on-thesis), while the *signing key* stays in the
//! `gl` identity dir the harness controls (never in agent env).

use std::path::PathBuf;

use tokio::process::Command;

use super::{CommitId, CommitMeta, RepoChange, RepoPath};
use crate::error::{DackError, Result};

pub struct GitCore {
    pub workdir: PathBuf,
    /// DID used to attribute harness-authored commits (e.g. reverts) when no per-commit
    /// author is supplied.
    pub default_author: String,
}

impl GitCore {
    pub fn new(workdir: impl Into<PathBuf>, default_author: impl Into<String>) -> Self {
        Self {
            workdir: workdir.into(),
            default_author: default_author.into(),
        }
    }

    /// Run `git -C <workdir> <args>` and return stdout, or an error carrying stderr.
    async fn git(&self, args: &[&str]) -> Result<String> {
        self.git_env(args, &[]).await
    }

    /// As [`git`](Self::git), but with extra environment overlaid — used by the signed
    /// `gitlawb://` push (`GITLAWB_KEY`/`GITLAWB_NODE`, read by the `git-remote-gitlawb` helper).
    async fn git_env(&self, args: &[&str], env: &[(&str, String)]) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.workdir).args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd
            .output()
            .await
            .map_err(|e| DackError::Repo(format!("git spawn ({}): {e}", args.join(" "))))?;
        if !out.status.success() {
            return Err(DackError::Repo(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    async fn head(&self) -> Result<CommitId> {
        Ok(CommitId(self.git(&["rev-parse", "HEAD"]).await?.trim().to_string()))
    }

    pub async fn read_file(&self, path: &RepoPath) -> Result<Vec<u8>> {
        let full = self.workdir.join(&path.0);
        tokio::fs::read(&full)
            .await
            .map_err(|e| DackError::Repo(format!("read {}: {e}", path.0)))
    }

    /// Write + commit a single file, attributed to `commit.author_did`. Idempotent: if the
    /// contents are unchanged, no empty commit is made and the current HEAD is returned.
    pub async fn write_file(
        &self,
        path: &RepoPath,
        contents: &[u8],
        commit: &CommitMeta,
    ) -> Result<CommitId> {
        let full = self.workdir.join(&path.0);
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| DackError::Repo(format!("mkdir {}: {e}", path.0)))?;
        }
        // ATOMIC replace: write to a unique temp in the same dir, then rename over the target. A
        // concurrent READER (the recall MCP runs in its OWN process, so it can't share a lock) thus
        // sees either the whole old file or the whole new one — never a half-written/truncated file.
        // The nonce keeps two writes to the same path from colliding on the temp name.
        static TMP_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = TMP_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::path::PathBuf::from(format!(
            "{}.tmp.{}.{nonce}",
            full.display(),
            std::process::id()
        ));
        tokio::fs::write(&tmp, contents)
            .await
            .map_err(|e| DackError::Repo(format!("write {}: {e}", path.0)))?;
        if let Err(e) = tokio::fs::rename(&tmp, &full).await {
            let _ = tokio::fs::remove_file(&tmp).await; // best-effort cleanup of the orphan temp
            return Err(DackError::Repo(format!("rename {}: {e}", path.0)));
        }
        self.git(&["add", "--", &path.0]).await?;

        // Nothing staged for this path (unchanged) → don't create an empty commit.
        let staged = self
            .git(&["status", "--porcelain", "--", &path.0])
            .await?;
        if staged.trim().is_empty() {
            return self.head().await;
        }
        self.commit_path(&path.0, &commit.message, &commit.author_did).await
    }

    /// Revert one file to its prior committed version — the ONE rollback case:
    /// a malformed skill/stimulus that broke a run. If the file has earlier history, check
    /// out the parent version; if it was only ever introduced once, remove it. The revert
    /// is itself committed (harness-authored).
    pub async fn revert_file(&self, path: &RepoPath) -> Result<CommitId> {
        let log = self
            .git(&["log", "-n", "2", "--format=%H", "--", &path.0])
            .await?;
        let hashes: Vec<&str> = log.lines().filter(|l| !l.is_empty()).collect();
        if hashes.len() >= 2 {
            // Restore the version from the commit before the latest one touching the path.
            self.git(&["checkout", hashes[1], "--", &path.0]).await?;
        } else {
            // Only one commit ever introduced it → the prior state is "absent".
            self.git(&["rm", "-f", "--", &path.0]).await?;
        }
        let author = self.default_author.clone();
        self.commit_path(&path.0, &format!("revert malformed {}", path.0), &author)
            .await
    }

    /// Delete a tracked file and commit the removal (harness-authored). Used by runlog retention to
    /// age off old day-files. A no-op (returns HEAD) if the path isn't tracked/present.
    pub async fn remove(&self, path: &RepoPath, message: &str, author: &str) -> Result<CommitId> {
        if self.git(&["rm", "-f", "--", &path.0]).await.is_err() {
            return self.head().await; // not tracked / already gone — nothing to do.
        }
        self.commit_path(&path.0, message, author).await
    }

    /// List files under `path` (filesystem walk, `.git` excluded) to `max_depth` relative
    /// to `path` — used by the `stimuli/` walker (depth ≤ 2) and memory `ls`.
    pub async fn list_dir(&self, path: &RepoPath, max_depth: usize) -> Result<Vec<RepoPath>> {
        let root = self.workdir.join(&path.0);
        let mut out = Vec::new();
        // Iterative walk to avoid async recursion. Stack of (abs_dir, depth).
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let mut rd = match tokio::fs::read_dir(&dir).await {
                Ok(rd) => rd,
                Err(_) => continue, // path may not exist yet
            };
            while let Some(entry) = rd
                .next_entry()
                .await
                .map_err(|e| DackError::Repo(format!("readdir: {e}")))?
            {
                let name = entry.file_name();
                if name == ".git" {
                    continue;
                }
                let abs = entry.path();
                let ft = entry
                    .file_type()
                    .await
                    .map_err(|e| DackError::Repo(format!("filetype: {e}")))?;
                if ft.is_dir() {
                    if depth + 1 <= max_depth {
                        stack.push((abs, depth + 1));
                    }
                } else if let Ok(rel) = abs.strip_prefix(&self.workdir) {
                    out.push(RepoPath(rel.to_string_lossy().into_owned()));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Commit a single pathspec attributed to `author_did` (author + committer), with no
    /// dependence on global git config.
    async fn commit_path(&self, path: &str, message: &str, author_did: &str) -> Result<CommitId> {
        let name = format!("user.name={author_did}");
        let email = format!("user.email={author_did}@dack");
        self.git(&[
            "-c", &name, "-c", &email, "commit", "-m", message, "--", path,
        ])
        .await?;
        self.head().await
    }

    /// The working-tree delta vs HEAD (untracked files included), as porcelain entries.
    /// `--porcelain=v1 -z`-free parse: each line is `XY <path>`; renames (`R`) report the
    /// destination. Empty result = clean tree.
    pub async fn status_porcelain(&self) -> Result<Vec<RepoChange>> {
        let out = self
            .git(&["status", "--porcelain", "--untracked-files=all"])
            .await?;
        let mut changes = Vec::new();
        for line in out.lines() {
            if line.len() < 4 {
                continue;
            }
            let code = line[..2].to_string();
            // Path starts at column 3. A rename prints `orig -> dest`; the dest is what changed.
            let rest = &line[3..];
            let raw = rest.rsplit(" -> ").next().unwrap_or(rest).trim();
            // git quotes paths containing special chars; strip the surrounding quotes.
            let path = raw.trim_matches('"').to_string();
            changes.push(RepoChange {
                code,
                path: RepoPath(path),
            });
        }
        Ok(changes)
    }

    /// Stage exactly `paths` (new/modified/deleted) and commit them attributed to `author_did`.
    /// `Ok(None)` when nothing was staged (the tree was already clean for these paths) — never
    /// an empty commit. The per-run sweep's committer for tool-driven `Write`/`Edit`.
    pub async fn commit_paths(
        &self,
        paths: &[RepoPath],
        message: &str,
        author_did: &str,
    ) -> Result<Option<CommitId>> {
        if paths.is_empty() {
            return Ok(None);
        }
        let mut add: Vec<&str> = vec!["add", "-A", "--"];
        add.extend(paths.iter().map(|p| p.0.as_str()));
        self.git(&add).await?;

        // Anything actually staged among these paths? (Avoid an empty commit.)
        let mut diff: Vec<&str> = vec!["diff", "--cached", "--name-only", "--"];
        diff.extend(paths.iter().map(|p| p.0.as_str()));
        if self.git(&diff).await?.trim().is_empty() {
            return Ok(None);
        }

        let name = format!("user.name={author_did}");
        let email = format!("user.email={author_did}@dack");
        let mut commit: Vec<&str> =
            vec!["-c", &name, "-c", &email, "commit", "-m", message, "--"];
        commit.extend(paths.iter().map(|p| p.0.as_str()));
        self.git(&commit).await?;
        Ok(Some(self.head().await?))
    }

    /// Discard a working-tree change, restoring it to HEAD: `checkout HEAD -- <path>` for a
    /// tracked file, or delete the file for an untracked one. The tripwire's revert — no commit.
    pub async fn restore_to_head(&self, change: &RepoChange) -> Result<()> {
        if change.is_untracked() {
            let full = self.workdir.join(&change.path.0);
            tokio::fs::remove_file(&full)
                .await
                .map_err(|e| DackError::Repo(format!("rm untracked {}: {e}", change.path.0)))?;
        } else {
            self.git(&["checkout", "HEAD", "--", &change.path.0]).await?;
        }
        Ok(())
    }

    /// `git push <remote> HEAD:<branch>` with `env` overlaid (the signed `gitlawb://` helper
    /// reads `GITLAWB_KEY`/`GITLAWB_NODE`). The branch is HEAD's current branch.
    pub async fn push(&self, remote: &str, env: &[(&str, String)]) -> Result<()> {
        self.push_with(remote, env, &[]).await
    }

    /// As [`push`](Self::push), but with extra `-c <cfg>` overrides prepended — used to inject an
    /// ephemeral HTTPS credential helper (`credential.helper=…`) for a plain-git token push, so the
    /// token rides in `env` (out of argv) while the helper that reads it rides in the config args.
    pub async fn push_with(
        &self,
        remote: &str,
        env: &[(&str, String)],
        config_args: &[String],
    ) -> Result<()> {
        let branch = self
            .git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .await?
            .trim()
            .to_string();
        let refspec = format!("HEAD:refs/heads/{branch}");
        let mut args: Vec<&str> = Vec::with_capacity(config_args.len() * 2 + 3);
        for c in config_args {
            args.push("-c");
            args.push(c);
        }
        args.push("push");
        args.push(remote);
        args.push(&refspec);
        self.git_env(&args, env).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_repo(tag: &str) -> PathBuf {
        // A throwaway repo under the system temp dir (no tempfile crate dependency).
        // `tag` keeps concurrent tests in the same binary (shared pid) from clobbering each other.
        let dir = std::env::temp_dir().join(format!("dack-gitcore-{}-{tag}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.name", "seed"],
            vec!["config", "user.email", "seed@dack"],
        ] {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(&args)
                .output()
                .await
                .unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn write_read_list_and_revert_round_trip() {
        let dir = init_repo("rtrip").await;
        let core = GitCore::new(&dir, "did:key:zSoul");
        let path = RepoPath("memory/log.md".into());
        let meta = |m: &str| CommitMeta {
            message: m.into(),
            author_did: "did:key:zSoul".into(),
        };

        // v1
        core.write_file(&path, b"first\n", &meta("first")).await.unwrap();
        assert_eq!(core.read_file(&path).await.unwrap(), b"first\n");

        // unchanged write → no new commit (idempotent)
        let h1 = core.write_file(&path, b"first\n", &meta("noop")).await.unwrap();
        let h1b = core.write_file(&path, b"first\n", &meta("noop")).await.unwrap();
        assert_eq!(h1, h1b, "unchanged content must not create a new commit");

        // v2
        core.write_file(&path, b"second\n", &meta("second")).await.unwrap();
        assert_eq!(core.read_file(&path).await.unwrap(), b"second\n");

        // list_dir sees the file
        let listed = core.list_dir(&RepoPath("memory".into()), 2).await.unwrap();
        assert!(listed.iter().any(|p| p.0 == "memory/log.md"));

        // revert v2 → back to v1 (the malformed-file rollback case)
        core.revert_file(&path).await.unwrap();
        assert_eq!(core.read_file(&path).await.unwrap(), b"first\n");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn status_sweep_and_tripwire_revert() {
        let dir = init_repo("sweep").await;
        let core = GitCore::new(&dir, "did:key:zSoul");
        // Seed a committed memory file so it has HEAD history.
        let mem = RepoPath("memory/log.md".into());
        core.write_file(&mem, b"seed\n", &CommitMeta { message: "seed".into(), author_did: "did:key:zSoul".into() })
            .await
            .unwrap();

        // Simulate a run's tool-driven writes straight to the working tree (no commit):
        // an ALLOWED memory append + a FORBIDDEN new skill file.
        tokio::fs::write(dir.join("memory/log.md"), b"seed\nagent note\n").await.unwrap();
        tokio::fs::create_dir_all(dir.join("skills/evil")).await.unwrap();
        tokio::fs::write(dir.join("skills/evil/SKILL.md"), b"injected\n").await.unwrap();

        // status sees both (one modified, one untracked).
        let changes = core.status_porcelain().await.unwrap();
        assert!(changes.iter().any(|c| c.path.0 == "memory/log.md" && !c.is_untracked()));
        let evil = changes.iter().find(|c| c.path.0 == "skills/evil/SKILL.md").unwrap().clone();
        assert!(evil.is_untracked(), "new skill file is untracked");

        // Tripwire: revert the forbidden untracked write → file gone, tree clean of it.
        core.restore_to_head(&evil).await.unwrap();
        assert!(!dir.join("skills/evil/SKILL.md").exists(), "forbidden write reverted");

        // Sweep: commit ONLY the allowlisted memory path → a real commit, attributed to Soul.
        let committed = core
            .commit_paths(&[mem.clone()], "run-x express: sweep", "did:key:zSoul")
            .await
            .unwrap();
        assert!(committed.is_some(), "the memory change was committed");
        assert_eq!(core.read_file(&mem).await.unwrap(), b"seed\nagent note\n");

        // Idempotent: a second sweep with nothing changed → no empty commit.
        assert!(core.commit_paths(&[mem], "noop", "did:key:zSoul").await.unwrap().is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn push_to_local_bare_remote_offline() {
        let base = std::env::temp_dir().join(format!("dack-push-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&base).await;
        let work = base.join("work");
        let bare = base.join("remote.git");
        tokio::fs::create_dir_all(&work).await.unwrap();
        // A local bare remote stands in for the gitlawb node (the push path, offline-tested).
        Command::new("git").args(["init", "-q", "--bare"]).arg(&bare).output().await.unwrap();
        for args in [vec!["init", "-q", "-b", "main"], vec!["config", "user.name", "s"], vec!["config", "user.email", "s@d"]] {
            Command::new("git").arg("-C").arg(&work).args(&args).output().await.unwrap();
        }
        Command::new("git").arg("-C").arg(&work).args(["remote", "add", "origin"]).arg(&bare).output().await.unwrap();

        let core = GitCore::new(&work, "did:key:zSoul");
        core.write_file(&RepoPath("SOUL.md".into()), b"I am.\n", &CommitMeta { message: "genesis".into(), author_did: "did:key:zSoul".into() })
            .await
            .unwrap();
        core.push("origin", &[]).await.unwrap();

        // The bare remote now has the branch at our HEAD.
        let head = core.head().await.unwrap();
        let remote_ref = Command::new("git").arg("-C").arg(&bare).args(["rev-parse", "refs/heads/main"]).output().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&remote_ref.stdout).trim(), head.0);

        let _ = tokio::fs::remove_dir_all(&base).await;
    }
}
