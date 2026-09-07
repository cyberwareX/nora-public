//! Gitlawb identity adapter — LEGACY backend, grounded against `gl` 0.3.8. Selected only when
//! `identities.backend: gitlawb`; the default is the native [`super::local`] backend (no external
//! tools). Kept for setups that already manage keys through the gitlawb `gl` CLI.
//!
//! Each role is a separate `gl` identity **dir** (`gl identity new --dir <dir>` writes an
//! `identity.pem`; the DID is `did:key:z6Mk…`). The **Soul dir is harness-only** — its key never
//! enters agent env; the harness invokes `gl identity sign --dir <soul>` to attest commits on the
//! duck's behalf. The Builder dir is env-forwardable.
//!
//! - `sign`  → `gl identity sign --dir <dir> <message>` → base64url Ed25519 signature.
//! - `did`   → resolved at construction via `gl identity show --dir <dir>`.
//! - `verify`→ shared local crypto ([`super::ed25519::verify_ed25519_did`]); `gl` 0.3.8 exposes no
//!   general message-verify command, and provenance is ours to check either way. A signature made by
//!   the native backend verifies here and vice-versa (same `did:key`, same base64url encoding).

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use tokio::process::Command;

use super::ed25519::verify_ed25519_did;
use super::{Did, IdentityProvider, IdentityRole, Signature};
use crate::error::{DackError, Result};

pub struct GitlawbIdentity {
    gl_bin: String,
    dirs: HashMap<IdentityRole, PathBuf>,
    dids: HashMap<IdentityRole, Did>,
}

impl GitlawbIdentity {
    /// Resolve the DID for each configured role dir up front (one `gl identity show` each),
    /// so `did()` can stay synchronous. Roles without a dir are simply absent.
    pub async fn resolve(
        gl_bin: impl Into<String>,
        dirs: HashMap<IdentityRole, PathBuf>,
    ) -> Result<Self> {
        let gl_bin = gl_bin.into();
        let mut dids = HashMap::new();
        for (role, dir) in &dirs {
            let did = gl_show(&gl_bin, dir).await?;
            dids.insert(*role, did);
        }
        Ok(Self { gl_bin, dirs, dids })
    }

    fn dir(&self, role: IdentityRole) -> Result<&PathBuf> {
        self.dirs
            .get(&role)
            .ok_or_else(|| DackError::Identity(format!("no identity dir for {role:?}")))
    }
}

async fn gl_show(gl_bin: &str, dir: &PathBuf) -> Result<Did> {
    let out = Command::new(gl_bin)
        .args(["identity", "show", "--dir"])
        .arg(dir)
        .output()
        .await
        .map_err(|e| DackError::Identity(format!("gl spawn: {e}")))?;
    if !out.status.success() {
        return Err(DackError::Identity(format!(
            "gl identity show: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(Did(String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

#[async_trait]
impl IdentityProvider for GitlawbIdentity {
    fn did(&self, role: IdentityRole) -> Option<&Did> {
        self.dids.get(&role)
    }

    async fn sign(&self, role: IdentityRole, payload: &[u8]) -> Result<Signature> {
        let dir = self.dir(role)?;
        // v1: messages are UTF-8 (commit attestations, `dack say` text). Binary payloads
        // would be base64url-wrapped before signing; not needed in v1.
        let message = std::str::from_utf8(payload)
            .map_err(|_| DackError::Identity("sign payload not UTF-8 (v1 limitation)".into()))?;
        let out = Command::new(&self.gl_bin)
            .args(["identity", "sign", "--dir"])
            .arg(dir)
            .arg(message)
            .output()
            .await
            .map_err(|e| DackError::Identity(format!("gl spawn: {e}")))?;
        if !out.status.success() {
            return Err(DackError::Identity(format!(
                "gl identity sign: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        // stdout = base64url signature. We store its ASCII bytes; verify decodes it.
        Ok(Signature(String::from_utf8_lossy(&out.stdout).trim().as_bytes().to_vec()))
    }

    async fn verify(&self, did: &Did, payload: &[u8], sig: &Signature) -> Result<bool> {
        // (operator_signed / `dack say`): a LOCAL crypto check, never a rhetorical one — shared with
        // the native backend so a signature from either verifies against the same `did:key`.
        verify_ed25519_did(did, payload, sig)
    }
}
