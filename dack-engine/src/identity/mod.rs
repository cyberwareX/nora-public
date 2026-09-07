//! Identity-provider seam. The core speaks **"sign as identity X"**;
//! Gitlawb specifics stay in the adapter so the corporate variant
//! (`CorporateIdentity`) is an adapter swap, not a harness refactor. Provenance is a
//! signature check (RFC 9421 / Ed25519), never a sentence the model evaluates
//! ("provenance beats persuasion").

use async_trait::async_trait;

use crate::error::Result;

/// `did:key:z6Mk...` — every actor (human or agent) is an Ed25519 keypair.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Did(pub String);

#[derive(Debug, Clone)]
pub struct Signature(pub Vec<u8>);

/// The three identities of the liability boundary:
///   - `Operator` — root authority; funds the VPS; legally responsible.
///   - `Soul`     — the actor's canonical self; key is **harness-only**, never in
///                  agent env; its compromise is narrative-ending.
///   - `Builder`  — recoverable identity for *outward* creation; key **is**
///                  env-forwarded; must NEVER gain soul-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdentityRole {
    Operator,
    Soul,
    Builder,
}

/// Sign / verify on behalf of a DID. v1 impl: [`gitlawb::GitlawbIdentity`].
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    fn did(&self, role: IdentityRole) -> Option<&Did>;

    /// Sign a payload as `role`. The Soul key never leaves the harness; the Builder
    /// key lives in forwarded env.
    async fn sign(&self, role: IdentityRole, payload: &[u8]) -> Result<Signature>;

    /// Verify a signature against a claimed DID. This is how `operator_signed` tier is
    /// established for `dack say` and (future) Settle triggers — a cryptographic check,
    /// never a rhetorical one.
    async fn verify(&self, did: &Did, payload: &[u8], sig: &Signature) -> Result<bool>;
}

pub mod ed25519;
pub mod gitlawb;
pub mod local;
