//! Canonical parse output shared by every dat family (docs/dats.md).
//!
//! Losslessness contract (D13): fields the audit/selection path queries are
//! typed; everything else — including attributes we have never seen — is
//! preserved in `attrs` maps (gotcha 8). The dat blob in CAS remains the
//! true canonical form; this model must merely never *drop* information.

use std::collections::BTreeMap;

/// Preserved key/value attributes, deterministically ordered.
pub type Attrs = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DatFile {
    pub header: DatHeader,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DatHeader {
    pub name: Option<String>,
    pub description: Option<String>,
    pub version: Option<String>,
    pub date: Option<String>,
    pub author: Option<String>,
    pub homepage: Option<String>,
    pub url: Option<String>,
    /// clrmamepro emitter hints (dats: forcemerging none|split|full,
    /// forcenodump obsolete|required|ignore, forcepacking zip|unzip),
    /// stored as written.
    pub force_merging: Option<String>,
    pub force_nodump: Option<String>,
    pub force_packing: Option<String>,
    /// Header-skipper detector reference (`clrmamepro header=` / cmpro
    /// `header`), resolved against [`crate::skipper`] detectors at import.
    pub detector: Option<String>,
    pub attrs: Attrs,
}

/// One game / machine / software entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub description: Option<String>,
    pub year: Option<String>,
    pub manufacturer: Option<String>,
    pub is_bios: bool,
    pub is_device: bool,
    pub is_mechanical: bool,
    pub runnable: bool,
    pub cloneof: Option<String>,
    pub romof: Option<String>,
    pub sampleof: Option<String>,
    /// No-Intro P/C XML extensions (dats gotcha 1: entry identity across
    /// revisions).
    pub id: Option<String>,
    pub cloneof_id: Option<String>,
    pub releases: Vec<Release>,
    /// MAME device_ref closure inputs, in document order (D31: captured now,
    /// closure queries land post-MVP).
    pub device_refs: Vec<String>,
    /// The flat claim list — the audit view, uniform across formats.
    pub claims: Vec<RomClaim>,
    /// Software-list part/dataarea structure (D13 losslessness). Claim
    /// references are indices into [`Entry::claims`], so the flat audit view
    /// and the lossless structure share one set of claim rows.
    pub parts: Vec<SoftwarePart>,
    pub attrs: Attrs,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: None,
            year: None,
            manufacturer: None,
            is_bios: false,
            is_device: false,
            is_mechanical: false,
            runnable: true,
            cloneof: None,
            romof: None,
            sampleof: None,
            id: None,
            cloneof_id: None,
            releases: Vec::new(),
            device_refs: Vec::new(),
            claims: Vec::new(),
            parts: Vec::new(),
            attrs: Attrs::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Release {
    pub name: String,
    pub region: String,
    pub language: Option<String>,
    pub date: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    Rom,
    /// CHD internal-data sha1, not a file hash (dats gotcha 3).
    Disk,
    Sample,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClaimStatus {
    #[default]
    Good,
    BadDump,
    /// Can never be satisfied; excluded from completeness math per
    /// forcenodump (dats gotcha 5).
    NoDump,
    Verified,
}

impl ClaimStatus {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "good" => Some(Self::Good),
            "baddump" => Some(Self::BadDump),
            "nodump" => Some(Self::NoDump),
            "verified" => Some(Self::Verified),
            _ => None,
        }
    }
}

/// One rom/disk/sample claim. Hash tuple is partial by design (dats
/// gotcha 2); zero-byte sizes and duplicate hashes are legal (gotchas 4/5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomClaim {
    pub kind: ClaimKind,
    /// Path within the set. May be empty for software-list load directives
    /// (`loadflag="continue"/"ignore"` rows carry no file name).
    pub name: String,
    pub size: Option<u64>,
    pub crc32: Option<[u8; 4]>,
    pub md5: Option<[u8; 16]>,
    pub sha1: Option<[u8; 20]>,
    pub sha256: Option<[u8; 32]>,
    pub status: ClaimStatus,
    pub mia: bool,
    pub optional: bool,
    pub merge_name: Option<String>,
    pub attrs: Attrs,
}

impl Default for RomClaim {
    fn default() -> Self {
        Self {
            kind: ClaimKind::Rom,
            name: String::new(),
            size: None,
            crc32: None,
            md5: None,
            sha1: None,
            sha256: None,
            status: ClaimStatus::Good,
            mia: false,
            optional: false,
            merge_name: None,
            attrs: Attrs::new(),
        }
    }
}

/// Software-list `part` (D13): interface + features + areas, lossless.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SoftwarePart {
    pub name: String,
    pub interface: String,
    pub features: Attrs,
    pub dataareas: Vec<DataArea>,
    pub diskareas: Vec<DiskArea>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DataArea {
    pub name: String,
    pub size: Option<u64>,
    pub width: Option<String>,
    pub endianness: Option<String>,
    /// Indices into [`Entry::claims`].
    pub claims: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiskArea {
    pub name: String,
    /// Indices into [`Entry::claims`].
    pub claims: Vec<usize>,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error(transparent)]
    Xml(#[from] quick_xml::Error),
    #[error(transparent)]
    Attr(#[from] quick_xml::events::attributes::AttrError),
    #[error("invalid dat at byte {pos}: {msg}")]
    Invalid { pos: u64, msg: String },
    #[error("unsupported dat format: {0}")]
    Unsupported(&'static str),
    #[error("unrecognized dat format")]
    UnknownFormat,
}

impl ParseError {
    pub(crate) fn invalid(pos: u64, msg: impl Into<String>) -> Self {
        Self::Invalid {
            pos,
            msg: msg.into(),
        }
    }
}

/// Parse a crc32 hex string. The wild strips leading zeros from CRCs in old
/// dats (dats gotcha 2), so short strings are left-padded to 8 digits;
/// md5/sha1/sha256 never appear truncated and are parsed exact-length only.
pub fn parse_crc32(s: &str) -> Option<[u8; 4]> {
    if s.is_empty() || s.len() > 8 {
        return None;
    }
    let mut padded = [b'0'; 8];
    padded[8 - s.len()..].copy_from_slice(s.as_bytes());
    let mut out = [0u8; 4];
    decode_hex(&padded, &mut out).then_some(out)
}

pub fn parse_md5(s: &str) -> Option<[u8; 16]> {
    parse_hex_exact(s)
}

pub fn parse_sha1(s: &str) -> Option<[u8; 20]> {
    parse_hex_exact(s)
}

pub fn parse_sha256(s: &str) -> Option<[u8; 32]> {
    parse_hex_exact(s)
}

fn parse_hex_exact<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    decode_hex(s.as_bytes(), &mut out).then_some(out)
}

fn decode_hex(src: &[u8], out: &mut [u8]) -> bool {
    for (i, chunk) in src.chunks_exact(2).enumerate() {
        let (Some(hi), Some(lo)) = (hex_val(chunk[0]), hex_val(chunk[1])) else {
            return false;
        };
        out[i] = (hi << 4) | lo;
    }
    true
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Parse a size that may be decimal or `0x`-prefixed hex (software-list
/// dataarea sizes appear both ways in MAME's tree).
pub fn parse_size(s: &str) -> Option<u64> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

/// XML boolean attributes: MAME and Logiqx write `yes`/`no`.
pub(crate) fn is_yes(s: &str) -> bool {
    s == "yes"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_pads_short_values_and_accepts_case() {
        assert_eq!(parse_crc32("deadBEEF"), Some([0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(parse_crc32("abc"), Some([0x00, 0x00, 0x0a, 0xbc]));
        assert_eq!(parse_crc32(""), None);
        assert_eq!(parse_crc32("123456789"), None);
        assert_eq!(parse_crc32("xyzw"), None);
    }

    #[test]
    fn strong_hashes_are_exact_length_only() {
        assert!(parse_md5(&"a".repeat(32)).is_some());
        assert!(parse_md5(&"a".repeat(30)).is_none());
        assert!(parse_sha1(&"0".repeat(40)).is_some());
        assert!(parse_sha256(&"F".repeat(64)).is_some());
        assert!(parse_sha256(&"g".repeat(64)).is_none());
    }

    #[test]
    fn sizes_parse_decimal_and_hex() {
        assert_eq!(parse_size("4194304"), Some(4_194_304));
        assert_eq!(parse_size("0x800000"), Some(0x0080_0000));
        assert_eq!(parse_size("0X10"), Some(16));
        assert_eq!(parse_size("banana"), None);
    }
}
