//! Guest-side crate for the `datboi:extractor@1` world (D89,
//! docs/worlds.md §vending).
//!
//! An author's whole job:
//!
//! ```ignore
//! use datboi_guest_extractor as x;
//!
//! struct Ex;
//! impl x::Guest for Ex { /* enumerate / extract */ }
//! x::export!(Ex);
//! ```
//!
//! `enumerate` returns the canonical-CBOR member list; the schema
//! constants and [`encode_members`] live HERE, beside the world they
//! describe; the host's decoder mirrors them
//! (datboi-runtime/src/extractor.rs). Added keys must be advisory-only
//! — old hosts ignore unknown keys (D64), so anything a host must
//! understand to execute correctly is a lane version, never a new key.

// no_std + alloc so no_std guests (ex-unrar owns its panic handler and
// heap) can link this crate without dragging std's runtime in. Tests
// keep std for the harness.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

// Nested in a module because `pub_export_macro` places the cabi macro
// at the crate root — generate! at the root would collide with its own
// re-export.
// Public because the `export!` macro expands in CONSUMER crates and
// addresses types through this path; consumers should still use the
// root re-exports below.
#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        world: "extractor",
        path: "../../wit/extractor/v1",
        // The streams package is ours (D89 shared-contract lane) —
        // generate its bindings here rather than mapping to an
        // external crate.
        generate_all,
        pub_export_macro: true,
        default_bindings_module: "datboi_guest_extractor::bindings",
    });
}

// The world's `use`s lift `ExtractRequest` and `File` to the bindings
// root; the resource types live in the streams package's module. One
// flat root for consumers either way.
pub use bindings::datboi::streams::types::{File, Sink};
pub use bindings::{ExtractRequest, Guest, export};

pub mod cbor;

/// Member-list CBOR schema (docs/worlds.md; mirrored by the host
/// decoder). Top level: `{1: [member...]}` — a map, not a bare array,
/// so archive-level advisory keys have somewhere to land later.
pub const ENUMERATION_KEY_MEMBERS: u64 = 1;

/// Member keys: `{1: ix, 2: name, 3: size, 4: packed-size, 5: crc32,
/// 6: solid}`. All six are always present; `solid` is uint 0/1 (each
/// truth value has exactly one encoding — the house canonical rule).
pub const MEMBER_KEY_IX: u64 = 1;
pub const MEMBER_KEY_NAME: u64 = 2;
pub const MEMBER_KEY_SIZE: u64 = 3;
pub const MEMBER_KEY_PACKED_SIZE: u64 = 4;
pub const MEMBER_KEY_CRC32: u64 = 5;
pub const MEMBER_KEY_SOLID: u64 = 6;

/// One archive member, as `enumerate` reports it. `ix` is the member's
/// stable identity within the ordered container list (files only,
/// listed order — directories, links and NTFS streams are not listed
/// and do not consume an index): derive recipes pin (containers, ix).
/// `name` is metadata, never an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Position in archive order among LISTED members.
    pub ix: u32,
    /// Member path as stored, converted to UTF-8 (lossy where the
    /// container's encoding allows unpaired garbage).
    pub name: String,
    /// Unpacked size in bytes.
    pub size: u64,
    /// Packed size in bytes (metadata; solid members share blocks).
    pub packed_size: u64,
    /// The container's declared CRC32 of the unpacked bytes (0 when the
    /// format carried none). A declaration, not a verification — the
    /// CAS hash on extraction is the identity (D4).
    pub crc32: u32,
    /// Member is part of a solid block (extraction decodes its solid
    /// predecessors first; cost, not semantics).
    pub solid: bool,
}

/// Encode a member list as the canonical-CBOR bytes `enumerate`
/// returns.
#[must_use]
pub fn encode_members(members: &[Member]) -> Vec<u8> {
    let items = members
        .iter()
        .map(|m| {
            cbor::Value::Map(vec![
                (MEMBER_KEY_IX, cbor::Value::Uint(u64::from(m.ix))),
                (MEMBER_KEY_NAME, cbor::Value::Text(m.name.clone())),
                (MEMBER_KEY_SIZE, cbor::Value::Uint(m.size)),
                (MEMBER_KEY_PACKED_SIZE, cbor::Value::Uint(m.packed_size)),
                (MEMBER_KEY_CRC32, cbor::Value::Uint(u64::from(m.crc32))),
                (MEMBER_KEY_SOLID, cbor::Value::Uint(u64::from(m.solid))),
            ])
        })
        .collect();
    cbor::encode(&cbor::Value::Map(vec![(
        ENUMERATION_KEY_MEMBERS,
        cbor::Value::Array(items),
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_encode_canonically() {
        let bytes = encode_members(&[Member {
            ix: 0,
            name: "a".into(),
            size: 3,
            packed_size: 2,
            crc32: 0,
            solid: false,
        }]);
        // {1: [{1:0, 2:"a", 3:3, 4:2, 5:0, 6:0}]}
        assert_eq!(
            bytes,
            vec![
                0xa1, 0x01, 0x81, 0xa6, 0x01, 0x00, 0x02, 0x61, b'a', 0x03, 0x03, 0x04, 0x02, 0x05,
                0x00, 0x06, 0x00
            ]
        );
        assert_eq!(encode_members(&[]), vec![0xa1, 0x01, 0x80]);
    }
}
