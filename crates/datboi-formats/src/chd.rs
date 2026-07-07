//! CHD v5 header parsing (docs/60-dats.md gotcha 3, 90-roadmap M1): MAME
//! dats identify disks by the CHD's *internal* sha1 — a hash of the
//! decompressed data+metadata, NOT of the file bytes — so audit needs the
//! header's declaration to connect a stored `.chd` to a disk claim.
//!
//! Header-only, no decompression: the declared sha1 is a *self-attestation*
//! by whatever wrote the file. Audit therefore grades header matches as
//! `probable` (ruled 2026-07-06, D44): evidence, never proof, until a real
//! decompressing verify exists (M2 chdman-port component).

/// Every CHD file starts with this magic, all versions.
pub const CHD_MAGIC: &[u8; 8] = b"MComprHD";

/// Bytes needed to parse a v5 header (fixed-size, from the MAME format).
pub const CHD_V5_HEADER_LEN: usize = 124;

/// A parsed CHD header. Only v5 carries the fields we use; other versions
/// are surfaced so ingest can report them rather than silently skipping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChdHeader {
    V5(ChdV5),
    /// A real CHD of a version we don't parse (v1–v4 layouts differ).
    Unsupported {
        version: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChdV5 {
    /// Uncompressed data size in bytes.
    pub logical_bytes: u64,
    /// sha1 of raw data only.
    pub raw_sha1: [u8; 20],
    /// sha1 of data + metadata — THE identity MAME dats reference.
    pub sha1: [u8; 20],
    /// Parent CHD's sha1 for delta CHDs; all-zero when standalone.
    pub parent_sha1: [u8; 20],
}

impl ChdV5 {
    /// Whether this CHD depends on a parent (delta dump).
    #[must_use]
    pub fn has_parent(&self) -> bool {
        self.parent_sha1 != [0u8; 20]
    }
}

/// Parse a CHD header from a file's first bytes. `None`: not a CHD at all.
/// `Some(Unsupported)`: CHD magic with a non-v5 version (or a v5 header too
/// short to carry its fixed fields, which real files never are).
#[must_use]
pub fn parse_header(prefix: &[u8]) -> Option<ChdHeader> {
    let rest = prefix.strip_prefix(CHD_MAGIC.as_slice())?;
    // magic(8) + length(4) + version(4), big-endian throughout.
    let version = u32::from_be_bytes(rest.get(4..8)?.try_into().ok()?);
    if version != 5 || prefix.len() < CHD_V5_HEADER_LEN {
        return Some(ChdHeader::Unsupported { version });
    }
    let field = |at: usize, len: usize| &prefix[at..at + len];
    Some(ChdHeader::V5(ChdV5 {
        logical_bytes: u64::from_be_bytes(field(32, 8).try_into().expect("fixed len")),
        raw_sha1: field(64, 20).try_into().expect("fixed len"),
        sha1: field(84, 20).try_into().expect("fixed len"),
        parent_sha1: field(104, 20).try_into().expect("fixed len"),
    }))
}

/// Build a synthetic v5 header (tests and fixture generation).
#[must_use]
pub fn synth_v5(logical_bytes: u64, raw_sha1: [u8; 20], sha1: [u8; 20]) -> Vec<u8> {
    let mut h = vec![0u8; CHD_V5_HEADER_LEN];
    h[..8].copy_from_slice(CHD_MAGIC);
    h[8..12].copy_from_slice(&(CHD_V5_HEADER_LEN as u32).to_be_bytes());
    h[12..16].copy_from_slice(&5u32.to_be_bytes());
    h[32..40].copy_from_slice(&logical_bytes.to_be_bytes());
    h[64..84].copy_from_slice(&raw_sha1);
    h[84..104].copy_from_slice(&sha1);
    // parent stays zero: standalone.
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v5_fields() {
        let bytes = synth_v5(1 << 30, [0xAA; 20], [0xBB; 20]);
        let ChdHeader::V5(v5) = parse_header(&bytes).expect("is a chd") else {
            panic!("expected v5");
        };
        assert_eq!(v5.logical_bytes, 1 << 30);
        assert_eq!(v5.raw_sha1, [0xAA; 20]);
        assert_eq!(v5.sha1, [0xBB; 20]);
        assert!(!v5.has_parent());
    }

    #[test]
    fn non_chd_and_old_versions() {
        assert_eq!(parse_header(b"PK\x03\x04 not a chd"), None);
        assert_eq!(parse_header(b"MCompr"), None); // truncated magic

        let mut v4 = synth_v5(0, [0; 20], [0; 20]);
        v4[12..16].copy_from_slice(&4u32.to_be_bytes());
        assert_eq!(
            parse_header(&v4),
            Some(ChdHeader::Unsupported { version: 4 })
        );
    }

    #[test]
    fn delta_chd_reports_parent() {
        let mut bytes = synth_v5(64, [1; 20], [2; 20]);
        bytes[104..124].copy_from_slice(&[3; 20]);
        let ChdHeader::V5(v5) = parse_header(&bytes).expect("is a chd") else {
            panic!("expected v5");
        };
        assert!(v5.has_parent());
    }
}
