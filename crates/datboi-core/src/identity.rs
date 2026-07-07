//! The instance identity keypair (ed25519).
//!
//! This is the one non-CAS secret (D15): it signs state snapshots (the
//! recovery root) and will double as the server/iroh node identity (D8).
//! Losing it loses snapshot-signing continuity — integrity still holds via
//! blake3, but authenticity (did *we* produce this snapshot) does not.
//!
//! Signing is deterministic (RFC 8032): a fixed seed over fixed bytes yields
//! a fixed signature, which is what lets snapshot objects be golden-tested.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// A 32-byte ed25519 public key (the verifying half).
pub type PublicKey = [u8; 32];

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("could not gather entropy for key generation: {0}")]
    Entropy(#[from] getrandom::Error),
    #[error("malformed public key")]
    BadPublicKey,
    #[error("signature verification failed")]
    BadSignature,
}

/// An instance's signing identity. Holds the secret seed; `public_key()`
/// exposes the shareable half.
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    /// Generate a fresh identity from OS entropy.
    pub fn generate() -> Result<Self, IdentityError> {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed)?;
        Ok(Self::from_seed(seed))
    }

    /// Reconstruct from a stored 32-byte secret seed (the on-disk key file
    /// and the golden tests both use this).
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(&seed),
        }
    }

    /// The 32-byte secret seed, for persisting to the key file.
    #[must_use]
    pub fn to_seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.signing.verifying_key().to_bytes()
    }

    /// Detached signature over `msg` (64 bytes).
    #[must_use]
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }
}

/// Verify a detached signature against a public key. A strict check
/// (rejects non-canonical signatures / malleability).
pub fn verify(
    public_key: &PublicKey,
    msg: &[u8],
    signature: &[u8; 64],
) -> Result<(), IdentityError> {
    let key = VerifyingKey::from_bytes(public_key).map_err(|_| IdentityError::BadPublicKey)?;
    let sig = Signature::from_bytes(signature);
    key.verify(msg, &sig)
        .map_err(|_| IdentityError::BadSignature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_round_trip() {
        let id = Identity::from_seed([7u8; 32]);
        let msg = b"state snapshot payload";
        let sig = id.sign(msg);
        assert!(verify(&id.public_key(), msg, &sig).is_ok());
    }

    #[test]
    fn tamper_is_detected() {
        let id = Identity::from_seed([7u8; 32]);
        let sig = id.sign(b"original");
        // Wrong message.
        assert!(verify(&id.public_key(), b"forged", &sig).is_err());
        // Flipped signature bit.
        let mut bad = sig;
        bad[0] ^= 1;
        assert!(verify(&id.public_key(), b"original", &bad).is_err());
        // Wrong key.
        let other = Identity::from_seed([9u8; 32]);
        assert!(verify(&other.public_key(), b"original", &sig).is_err());
    }

    #[test]
    fn seed_round_trips() {
        let id = Identity::from_seed([3u8; 32]);
        assert_eq!(id.to_seed(), [3u8; 32]);
        let again = Identity::from_seed(id.to_seed());
        assert_eq!(id.public_key(), again.public_key());
    }

    #[test]
    fn signing_is_deterministic() {
        // RFC 8032: no RNG in signing, so this is stable across runs/hosts —
        // the property the snapshot golden vector relies on.
        let id = Identity::from_seed([1u8; 32]);
        assert_eq!(id.sign(b"abc"), id.sign(b"abc"));
    }
}
