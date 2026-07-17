//! The instance identity: one root secret, purpose-derived keys (D99).
//!
//! The on-disk `identity.key` holds a 32-byte **root secret** — the one
//! non-CAS secret (D15). The root **signs and authenticates nothing
//! directly**: every protocol key is a domain-separated derivation from it
//! (`blake3::derive_key` with a unique context string), so no single key
//! ever does double duty and cross-protocol key confusion is structurally
//! impossible (D99, refining D8's "the keypair doubles as the iroh key").
//!
//! Two derived keys today:
//! - **snapshot-signing** (ed25519) — signs state snapshots, the recovery
//!   root; losing the root loses signing continuity (integrity still holds
//!   via blake3, but authenticity does not).
//! - **iroh-identity** (ed25519) — the iroh `SecretKey`, whose public half
//!   is the EndpointId peers know us by and put on ACLs (D8).
//!
//! Future uses get their own context label; the root gains no new powers.
//! Derivation and ed25519 signing are both deterministic, which is what
//! lets snapshot objects be golden-tested.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// A 32-byte ed25519 public key (the verifying half).
pub type PublicKey = [u8; 32];

/// blake3 KDF context strings — globally unique, purpose- and
/// version-scoped (the derive-key convention). Changing one re-keys that
/// purpose; they are effectively at-rest constants.
const CTX_SNAPSHOT: &str = "datboi instance-identity 2026 snapshot-signing v1";
const CTX_IROH: &str = "datboi instance-identity 2026 iroh-identity v1";

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("could not gather entropy for key generation: {0}")]
    Entropy(#[from] getrandom::Error),
    #[error("malformed public key")]
    BadPublicKey,
    #[error("signature verification failed")]
    BadSignature,
}

/// An instance's identity: the root secret, plus derivations of it. The
/// root itself is never used as a signing or handshake key.
pub struct Identity {
    root: [u8; 32],
}

impl Identity {
    /// Generate a fresh identity from OS entropy.
    pub fn generate() -> Result<Self, IdentityError> {
        let mut root = [0u8; 32];
        getrandom::getrandom(&mut root)?;
        Ok(Self::from_seed(root))
    }

    /// Reconstruct from the stored 32-byte root secret (the on-disk key
    /// file and the golden tests both use this).
    #[must_use]
    pub fn from_seed(root: [u8; 32]) -> Self {
        Self { root }
    }

    /// The 32-byte root secret, for persisting to the key file.
    #[must_use]
    pub fn to_seed(&self) -> [u8; 32] {
        self.root
    }

    /// Derive a purpose-scoped ed25519 signing key. Private: callers reach
    /// a specific purpose through a named accessor, never an ad-hoc label.
    fn derive(&self, context: &str) -> SigningKey {
        SigningKey::from_bytes(&blake3::derive_key(context, &self.root))
    }

    /// The snapshot-signing verifying key (what a statesnap embeds and
    /// recovery pins to the local identity, D43).
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.derive(CTX_SNAPSHOT).verifying_key().to_bytes()
    }

    /// Detached signature over `msg` (64 bytes) by the snapshot-signing
    /// key. The root never signs.
    #[must_use]
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.derive(CTX_SNAPSHOT).sign(msg).to_bytes()
    }

    /// The 32-byte seed for the iroh `SecretKey` (`SecretKey::from_bytes`).
    /// Domain-separated from snapshot-signing: the same root, a different,
    /// unrelated ed25519 key — the iroh handshake can never mint or verify
    /// anything in the snapshot plane, nor vice versa (D99).
    #[must_use]
    pub fn iroh_secret(&self) -> [u8; 32] {
        self.derive(CTX_IROH).to_bytes()
    }

    /// The iroh public identity (the EndpointId peers know us by / ACL, D8).
    #[must_use]
    pub fn iroh_public(&self) -> PublicKey {
        self.derive(CTX_IROH).verifying_key().to_bytes()
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
        // RFC 8032 + a deterministic KDF: stable across runs/hosts — the
        // property the snapshot golden vector relies on.
        let id = Identity::from_seed([1u8; 32]);
        assert_eq!(id.sign(b"abc"), id.sign(b"abc"));
    }

    #[test]
    fn derived_keys_are_domain_separated_and_stable(){
        // D99: the root signs nothing; snapshot and iroh keys are distinct
        // derivations of it, each deterministic, neither equal to the root.
        let id = Identity::from_seed([5u8; 32]);
        assert_ne!(id.public_key(), id.iroh_public(), "snapshot ≠ iroh key");
        assert_ne!(id.iroh_secret(), id.to_seed(), "iroh key ≠ the raw root");
        // Deterministic across reconstructions from the same root.
        let again = Identity::from_seed([5u8; 32]);
        assert_eq!(id.iroh_secret(), again.iroh_secret());
        assert_eq!(id.public_key(), again.public_key());
        // A signature by the snapshot key does not verify under the iroh
        // key — the confusion D99 forecloses.
        let sig = id.sign(b"snapshot payload");
        assert!(verify(&id.public_key(), b"snapshot payload", &sig).is_ok());
        assert!(verify(&id.iroh_public(), b"snapshot payload", &sig).is_err());
    }
}
