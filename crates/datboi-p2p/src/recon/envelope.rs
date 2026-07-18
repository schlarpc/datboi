//! The recon wire envelope (D103): request and response header are
//! length-prefixed postcard messages; the payload after them stays the
//! raw coded-symbol stream ([`crate::riblt`]).
//!
//! Encoding registers (the D103 rule): a wire protocol's ENVELOPE —
//! heterogeneous, evolvable, tiny control-plane data — is a serialized
//! struct (postcard, the same envelope/stream split iroh-blobs itself
//! makes); homogeneous record streams are hand-rolled fixed binary
//! (D100); identity bytes never get near a macro (D18/D69). The serde
//! derives in THIS module are the entire scope of the D69 refinement
//! inside this crate.
//!
//! Framing: `[body_len: u16 LE][postcard body]`. The prefix is required
//! on the request because the initiator's send half stays open for the
//! stop signal, so FIN cannot frame it; the response header gets the
//! same treatment so both envelopes read identically.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::riblt::CODED_SYMBOL_LEN;

/// Cap on one envelope's postcard body. Real envelopes are a few bytes;
/// the cap bounds what a reader will allocate for a garbage length.
pub const MAX_ENVELOPE_LEN: usize = 4096;

/// What a responder will reconcile — the request enum, which IS the
/// scope registry (D103): variants are append-only and never
/// renumbered (the postcard discriminant is the wire tag; goldens pin
/// it), each declares its argument payload here and its symbol width
/// in [`Scope::frame_len`]. A non-32 width owes its own riblt goldens
/// before becoming surface (D100 amendment). The symmetric-prior
/// convention: a scope is ONE set definition evaluated on both
/// databases — the responder advertises it, the initiator runs the
/// same query locally as its decoder prior. A scope wanting an
/// asymmetric prior is a design smell to stop at (D102's roots scope
/// bends it knowingly — the sync walk mops up the asymmetry).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// Meta-blob hashes of non-Failed affine builtin `assemble@1`
    /// routes ([`datboi_index::Db::affine_recipe_objects`]) — the D100
    /// transfer-optimization plane.
    AffineRecipes,
    /// Resident Data-namespace blobs with no non-Failed producing
    /// route ([`datboi_index::Db::root_blobs`]) — the D102 completeness
    /// plane. With the plans this covers the holdings by construction:
    /// every blob is underived (here) or derived (reachable from a
    /// plan).
    RootBlobs,
}

impl Scope {
    /// The scope's coded-symbol record length, a protocol constant
    /// (one width per stream — D100 amendment). Echoed in the
    /// `Accepted` header — redundant, but it lets a tool skip a stream
    /// it doesn't understand.
    #[must_use]
    pub fn frame_len(self) -> u32 {
        match self {
            // Every current scope reconciles blake3-shaped symbols.
            Scope::AffineRecipes | Scope::RootBlobs => CODED_SYMBOL_LEN as u32,
        }
    }
}

/// The response header. Errors are HEADER-TIME ONLY: after `Accepted`,
/// the only failure signal is a QUIC stream reset — in-band trailers
/// would need escape sequences inside the frame stream, destroying the
/// every-record-parses property (D103).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// The scope is served: its set size, then `frame_len`-byte coded
    /// symbols follow until stop, stream closure, or the drain cap.
    Accepted { set_size: u64, frame_len: u32 },
    /// The request was understood not to be servable. A bare u16, not
    /// an enum, so an initiator can NAME a code a newer responder sent
    /// instead of failing to parse the refusal.
    Refused { code: u16 },
}

/// The request didn't parse as a [`Scope`] this responder speaks — a
/// newer peer's variant arrives as exactly this (unknown postcard
/// discriminant). Codes are append-only.
pub const REFUSED_UNKNOWN_SCOPE: u16 = 1;

/// Encode one envelope: `[body_len: u16 LE][postcard body]`.
///
/// # Panics
/// Never for real envelopes (a few bytes); debug-asserts the cap.
#[must_use]
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = postcard::to_stdvec(msg).expect("envelope types serialize infallibly");
    debug_assert!(body.len() <= MAX_ENVELOPE_LEN);
    let mut out = Vec::with_capacity(2 + body.len());
    out.extend_from_slice(
        &u16::try_from(body.len())
            .expect("envelope fits u16")
            .to_le_bytes(),
    );
    out.extend_from_slice(&body);
    out
}

/// Decode one envelope body (the bytes AFTER the length prefix).
/// Rejects trailing bytes — an envelope is exactly one message.
pub fn decode<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T> {
    postcard::from_bytes(body).context("recon envelope did not parse")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The append-only registry discipline, enforced: these are WIRE
    /// bytes. A variant reorder, rename-with-reorder, or discriminant
    /// shift breaks this test before it breaks a peer. New variants
    /// append new pins; existing pins never change.
    #[test]
    fn envelope_wire_bytes_are_pinned() {
        // Requests: len=1, then the scope discriminant.
        assert_eq!(encode(&Scope::AffineRecipes), [1, 0, 0]);
        assert_eq!(encode(&Scope::RootBlobs), [1, 0, 1]);
        // Accepted: discriminant 0, set_size varint, frame_len varint.
        assert_eq!(
            encode(&Response::Accepted {
                set_size: 300,
                frame_len: 48
            }),
            [4, 0, 0, 0xAC, 0x02, 48]
        );
        // Refused: discriminant 1, code varint.
        assert_eq!(
            encode(&Response::Refused {
                code: REFUSED_UNKNOWN_SCOPE
            }),
            [2, 0, 1, 1]
        );
    }

    #[test]
    fn envelopes_round_trip() {
        for scope in [Scope::AffineRecipes, Scope::RootBlobs] {
            let bytes = encode(&scope);
            assert_eq!(decode::<Scope>(&bytes[2..]).unwrap(), scope);
        }
        let resp = Response::Accepted {
            set_size: u64::MAX,
            frame_len: 48,
        };
        let bytes = encode(&resp);
        assert_eq!(decode::<Response>(&bytes[2..]).unwrap(), resp);
    }

    /// An unknown discriminant (a newer peer's scope) fails to decode —
    /// the responder maps exactly this onto `REFUSED_UNKNOWN_SCOPE`.
    #[test]
    fn unknown_scope_discriminant_is_a_parse_error() {
        assert!(decode::<Scope>(&[99]).is_err());
    }

    /// Trailing bytes are rejected: one envelope, one message.
    #[test]
    fn trailing_bytes_are_rejected() {
        assert!(decode::<Scope>(&[0, 0xFF]).is_err());
    }
}
