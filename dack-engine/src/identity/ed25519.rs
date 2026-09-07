//! Pure Ed25519 / `did:key` crypto shared by every [`IdentityProvider`](super::IdentityProvider)
//! backend. No I/O, no external tools — just the `did:key` codec and signature verify. Both the
//! native [`local`](super::local) backend and the legacy [`gitlawb`](super::gitlawb) adapter build on
//! this, so the wire formats (a `did:key:z6Mk…` and a base64url signature) are defined in exactly one
//! place. Provenance is this local crypto check, never a sentence the model evaluates.

use super::{Did, Signature};
use crate::error::{DackError, Result};

/// A raw 32-byte Ed25519 public key → its canonical `did:key:z6Mk…`. The body after `z` is
/// multibase base58btc of the `ed25519-pub` multicodec prefix (`0xed 0x01`) followed by the key.
pub fn ed25519_pub_to_did(pubkey: [u8; 32]) -> Did {
    let mut multicodec = Vec::with_capacity(34);
    multicodec.extend_from_slice(&[0xed, 0x01]);
    multicodec.extend_from_slice(&pubkey);
    Did(format!("did:key:z{}", bs58::encode(multicodec).into_string()))
}

/// The stored signature format: base64url (no pad) of the 64 raw signature bytes, as ASCII bytes —
/// what [`verify_ed25519_did`] decodes. Matches what `gl identity sign` emitted, so the two backends
/// interoperate (a `dack say` signed by either verifies against the same `did:key`).
pub fn encode_signature(sig: [u8; 64]) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sig)
        .into_bytes()
}

/// Verify an Ed25519 signature over `payload` against the key encoded in a `did:key`. The signature
/// is the ASCII of the base64 string a signer emits (we decode liberally: url-safe/standard,
/// padded/unpadded). Returns `Ok(false)` for a well-formed-but-wrong signature; `Err` only when the
/// DID or signature is structurally undecodable.
pub fn verify_ed25519_did(did: &Did, payload: &[u8], sig: &Signature) -> Result<bool> {
    use ed25519_dalek::{Signature as Ed25519Sig, Verifier, VerifyingKey};

    let pubkey = did_key_to_ed25519(&did.0)?;
    let verifying = VerifyingKey::from_bytes(&pubkey)
        .map_err(|e| DackError::Identity(format!("did pubkey not a valid ed25519 point: {e}")))?;
    let sig_bytes = decode_signature_64(&sig.0)?;
    let signature = Ed25519Sig::from_bytes(&sig_bytes);
    Ok(verifying.verify(payload, &signature).is_ok())
}

/// `did:key:z6Mk…` → the 32-byte Ed25519 public key. Inverse of [`ed25519_pub_to_did`].
pub fn did_key_to_ed25519(did: &str) -> Result<[u8; 32]> {
    let body = did
        .strip_prefix("did:key:")
        .ok_or_else(|| DackError::Identity(format!("not a did:key: `{did}`")))?;
    let b58 = body
        .strip_prefix('z')
        .ok_or_else(|| DackError::Identity(format!("did:key not base58btc (`z…`): `{did}`")))?;
    let bytes = bs58::decode(b58)
        .into_vec()
        .map_err(|e| DackError::Identity(format!("did:key base58 decode: {e}")))?;
    // 2-byte multicodec prefix (0xed 0x01 = ed25519-pub) + 32-byte key.
    if bytes.len() != 34 || bytes[0] != 0xed || bytes[1] != 0x01 {
        return Err(DackError::Identity(format!(
            "did:key not an ed25519-pub multicodec (len {}, prefix {:02x?})",
            bytes.len(),
            &bytes[..bytes.len().min(2)]
        )));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes[2..34]);
    Ok(key)
}

/// Decode the stored signature (ASCII of a base64 string) into the 64 raw Ed25519 bytes, trying the
/// common base64 alphabets/padding so we don't couple to any one signer's exact choice.
pub fn decode_signature_64(raw: &[u8]) -> Result<[u8; 64]> {
    use base64::Engine;
    let s = std::str::from_utf8(raw)
        .map_err(|_| DackError::Identity("signature not UTF-8 base64".into()))?
        .trim();
    let engines: [&base64::engine::GeneralPurpose; 4] = [
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        &base64::engine::general_purpose::URL_SAFE,
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        &base64::engine::general_purpose::STANDARD,
    ];
    for engine in engines {
        if let Ok(bytes) = engine.decode(s) {
            if bytes.len() == 64 {
                let mut sig = [0u8; 64];
                sig.copy_from_slice(&bytes);
                return Ok(sig);
            }
        }
    }
    Err(DackError::Identity(format!(
        "signature did not base64-decode to 64 bytes (`{}…`)",
        &s[..s.len().min(12)]
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};

    /// Build a `did:key` + a base64url signature from a fixed key — exercises the verify path without
    /// any signer binary, and pins the round-trip between [`ed25519_pub_to_did`] / [`encode_signature`]
    /// and the decode/verify side.
    fn did_and_sig(secret: [u8; 32], message: &[u8]) -> (Did, Signature) {
        let sk = SigningKey::from_bytes(&secret);
        let did = ed25519_pub_to_did(sk.verifying_key().to_bytes());
        let sig = sk.sign(message);
        (did, Signature(encode_signature(sig.to_bytes())))
    }

    #[test]
    fn verify_accepts_a_genuine_signature() {
        let msg = b"buy nothing today, duck";
        let (did, sig) = did_and_sig([7u8; 32], msg);
        assert!(verify_ed25519_did(&did, msg, &sig).unwrap());
    }

    #[test]
    fn verify_rejects_a_tampered_payload() {
        let (did, sig) = did_and_sig([7u8; 32], b"the real instruction");
        assert!(!verify_ed25519_did(&did, b"a forged instruction", &sig).unwrap());
    }

    #[test]
    fn verify_rejects_a_wrong_signer() {
        let msg = b"signed by someone else";
        let (_their_did, their_sig) = did_and_sig([9u8; 32], msg);
        let (operator_did, _) = did_and_sig([7u8; 32], msg);
        assert!(!verify_ed25519_did(&operator_did, msg, &their_sig).unwrap());
    }

    #[test]
    fn verify_errors_on_a_malformed_did() {
        let (_, sig) = did_and_sig([7u8; 32], b"x");
        assert!(verify_ed25519_did(&Did("did:web:example.com".into()), b"x", &sig).is_err());
    }

    #[test]
    fn did_key_round_trips_through_pub_and_back() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        let did = ed25519_pub_to_did(pk);
        assert!(did.0.starts_with("did:key:z6Mk"));
        assert_eq!(did_key_to_ed25519(&did.0).unwrap(), pk);
    }
}
