//! Native Ed25519 identity backend — the default. Reads each role's PKCS8 `identity.pem` directly,
//! derives the `did:key` from the public key, and signs with `ed25519-dalek` in-process. No external
//! tools: a fresh deployer needs only `dack keygen` (below) — NOT the gitlawb `gl` CLI (that legacy
//! adapter lives in [`super::gitlawb`] and stays available via `identities.backend: gitlawb`).
//!
//! The Soul key never enters agent env (the harness holds it); the Builder key is env-forwardable.
//! The `did:key` derivation is deterministic, so a key generated here (or by `gl` before) resolves to
//! the exact same DID — the two backends are drop-in interchangeable for the same `identity.pem`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::{Signer, SigningKey};
use pkcs8::LineEnding;

use super::ed25519::{ed25519_pub_to_did, encode_signature, verify_ed25519_did};
use super::{Did, IdentityProvider, IdentityRole, Signature};
use crate::error::{DackError, Result};

pub struct LocalIdentity {
    keys: HashMap<IdentityRole, SigningKey>,
    dids: HashMap<IdentityRole, Did>,
}

impl LocalIdentity {
    /// Load the PKCS8 `identity.pem` for each configured role dir and derive its `did:key` up front,
    /// so `did()` stays synchronous. Roles without a dir are simply absent. Fails closed if a
    /// configured key is missing or unparseable (a broken operator key must not boot as "no auth").
    pub fn resolve(dirs: HashMap<IdentityRole, PathBuf>) -> Result<Self> {
        let mut keys = HashMap::new();
        let mut dids = HashMap::new();
        for (role, dir) in dirs {
            let sk = load_pem(&dir)?;
            let did = ed25519_pub_to_did(sk.verifying_key().to_bytes());
            keys.insert(role, sk);
            dids.insert(role, did);
        }
        Ok(Self { keys, dids })
    }

    fn key(&self, role: IdentityRole) -> Result<&SigningKey> {
        self.keys
            .get(&role)
            .ok_or_else(|| DackError::Identity(format!("no identity key for {role:?}")))
    }
}

fn pem_path(dir: &Path) -> PathBuf {
    dir.join("identity.pem")
}

fn load_pem(dir: &Path) -> Result<SigningKey> {
    let path = pem_path(dir);
    let pem = std::fs::read_to_string(&path)
        .map_err(|e| DackError::Identity(format!("read {}: {e}", path.display())))?;
    SigningKey::from_pkcs8_pem(&pem)
        .map_err(|e| DackError::Identity(format!("parse {} as PKCS8 Ed25519: {e}", path.display())))
}

/// Generate a fresh Ed25519 identity into `<dir>/identity.pem` (mode 0600) and return its `did:key`.
/// **Refuses to overwrite** an existing key — a lost soul/operator key is unrecoverable, so
/// clobbering it must be a deliberate `rm` first. This is what `dack keygen` calls.
pub fn generate(dir: &Path) -> Result<Did> {
    let path = pem_path(dir);
    if path.exists() {
        return Err(DackError::Identity(format!(
            "{} already exists — refusing to overwrite an identity key (rm it first if you really mean to)",
            path.display()
        )));
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| DackError::Identity(format!("mkdir {}: {e}", dir.display())))?;
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| DackError::Identity(format!("csprng: {e}")))?;
    let sk = SigningKey::from_bytes(&seed);
    let pem = sk
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| DackError::Identity(format!("encode PKCS8: {e}")))?;
    write_private(&path, pem.as_bytes())?;
    Ok(ed25519_pub_to_did(sk.verifying_key().to_bytes()))
}

/// Write the private key with `create_new` + 0600 up front (the secret bytes never touch a
/// world-readable file, even briefly).
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| DackError::Identity(format!("create {}: {e}", path.display())))?;
    f.write_all(bytes)
        .map_err(|e| DackError::Identity(format!("write {}: {e}", path.display())))
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)
        .map_err(|e| DackError::Identity(format!("write {}: {e}", path.display())))
}

#[async_trait]
impl IdentityProvider for LocalIdentity {
    fn did(&self, role: IdentityRole) -> Option<&Did> {
        self.dids.get(&role)
    }

    async fn sign(&self, role: IdentityRole, payload: &[u8]) -> Result<Signature> {
        let sk = self.key(role)?;
        Ok(Signature(encode_signature(sk.sign(payload).to_bytes())))
    }

    async fn verify(&self, did: &Did, payload: &[u8], sig: &Signature) -> Result<bool> {
        verify_ed25519_did(did, payload, sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// generate → load → the DID is stable, and a signature by the loaded key verifies against it.
    /// This is the whole native path (keygen + boot + `dack say`) without any external tool.
    #[tokio::test]
    async fn generate_then_load_sign_verify_round_trips() {
        let dir = std::env::temp_dir().join(format!("dack-idtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let did_gen = generate(&dir).expect("generate");
        assert!(did_gen.0.starts_with("did:key:z6Mk"));

        // A second generate refuses to clobber.
        assert!(generate(&dir).is_err());

        // 0600 on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(pem_path(&dir)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let mut dirs = HashMap::new();
        dirs.insert(IdentityRole::Operator, dir.clone());
        let id = LocalIdentity::resolve(dirs).expect("resolve");
        assert_eq!(id.did(IdentityRole::Operator).unwrap(), &did_gen);

        let msg = b"dack say: buy nothing today";
        let sig = id.sign(IdentityRole::Operator, msg).await.unwrap();
        assert!(id.verify(&did_gen, msg, &sig).await.unwrap());
        assert!(!id.verify(&did_gen, b"forged", &sig).await.unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
