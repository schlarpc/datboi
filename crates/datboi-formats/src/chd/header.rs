//! CHD header + hunk-map parsing for every shipped version (v1–v5).
//!
//! All five layouts are fixed-size and big-endian. What differs is which
//! digests a version carries and how its hunk map is stored:
//!
//! | ver | header | map entry | digests |
//! |-----|--------|-----------|---------|
//! | 1   | 76     | 8 B packed at 76  | md5 (raw only) |
//! | 2   | 80     | 8 B packed at 80  | md5 (raw only) |
//! | 3   | 120    | 16 B at 120       | md5 + sha1 (raw only) |
//! | 4   | 108    | 16 B at 108       | sha1 (raw+meta) + rawsha1 |
//! | 5   | 124    | huffman-coded at `map_offset` | rawsha1 + sha1 (raw+meta) |
//!
//! The version gap matters for audit, not just for parsing: a v1/v2 CHD
//! declares no sha1 at all, so it can never answer a modern MAME `disk`
//! claim; a v3 CHD's `sha1` covers the raw data only (metadata is
//! outside it); v4 and v5 declare the combined raw+metadata sha1 that
//! dats actually reference. [`ChdHeader::declared_disk_sha1`] is the one
//! place that knows which field a version means by "the disk's sha1".

use super::ChdError;

/// Every CHD file starts with this magic, all versions.
pub const CHD_MAGIC: &[u8; 8] = b"MComprHD";

/// Bytes needed to parse a v5 header (fixed-size, from the MAME format).
/// Also the longest header of any version, so one read of this many
/// bytes classifies any CHD — ingest's single head sniff relies on that.
pub const CHD_V5_HEADER_LEN: usize = 124;

pub(crate) const CHD_V1_HEADER_LEN: usize = 76;
pub(crate) const CHD_V2_HEADER_LEN: usize = 80;
pub(crate) const CHD_V3_HEADER_LEN: usize = 120;
pub(crate) const CHD_V4_HEADER_LEN: usize = 108;

/// v1's sector size is not in the header; v2 added `seclen` for it.
pub(crate) const CHD_V1_SECTOR_SIZE: u32 = 512;

/// `flags` bit 0 (v1–v4): the file is a delta against a parent CHD.
const CHDFLAGS_HAS_PARENT: u32 = 0x0000_0001;

/// Codec tags. v5 stores these FourCCs directly; v1–v4 store a small
/// enum that `legacy_codec` maps onto the same vocabulary, so one
/// dispatch table serves every version.
pub const CODEC_NONE: u32 = 0;
pub const CODEC_ZLIB: u32 = fourcc(b"zlib");
pub const CODEC_LZMA: u32 = fourcc(b"lzma");
pub const CODEC_HUFFMAN: u32 = fourcc(b"huff");
pub const CODEC_FLAC: u32 = fourcc(b"flac");
pub const CODEC_CD_ZLIB: u32 = fourcc(b"cdzl");
pub const CODEC_CD_LZMA: u32 = fourcc(b"cdlz");
pub const CODEC_CD_FLAC: u32 = fourcc(b"cdfl");
pub const CODEC_AVHUFF: u32 = fourcc(b"avhu");

#[must_use]
pub const fn fourcc(tag: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*tag)
}

/// Render a codec tag the way MAME spells it, for operator-facing
/// detail strings ("hunk 41 needs codec `cdfl`").
#[must_use]
pub fn codec_name(codec: u32) -> String {
    if codec == CODEC_NONE {
        return "none".into();
    }
    let bytes = codec.to_be_bytes();
    if bytes.iter().all(u8::is_ascii_graphic) {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("0x{codec:08x}")
    }
}

/// v1–v4 `compression` field → the v5 codec vocabulary. `ZLIB_PLUS` is
/// the same DEFLATE stream as `ZLIB` (the "+" was a compressor-side
/// setting), so both decode identically.
fn legacy_codec(compression: u32) -> Option<u32> {
    match compression {
        0 => Some(CODEC_NONE),
        1 | 2 => Some(CODEC_ZLIB),
        3 => Some(CODEC_AVHUFF),
        _ => None,
    }
}

/// A parsed CHD header, flattened across versions. Fields a version
/// does not carry are `None` — never faked, so a decision can always
/// tell "this file does not declare that" from "it declares zero".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChdHeader {
    pub version: u32,
    /// Uncompressed data size in bytes (derived from the geometry for
    /// v1/v2, which have no logical-size field).
    pub logical_bytes: u64,
    pub hunk_bytes: u32,
    /// v5's compression unit; `hunk_bytes` for older versions, which
    /// have no sub-hunk addressing.
    pub unit_bytes: u32,
    pub total_hunks: u32,
    /// Where the hunk map starts. Fixed at the header length for
    /// v1–v4; a real field for v5.
    pub map_offset: u64,
    /// Head of the metadata chain (v3+; v1/v2 have no metadata).
    pub meta_offset: u64,
    /// The four v5 codec slots; v1–v4 fill slot 0 and leave the rest
    /// `CODEC_NONE`.
    pub compressors: [u32; 4],
    /// sha1 of raw data only (v3 stores it in `sha1`; v4/v5 in
    /// `rawsha1`).
    pub raw_sha1: Option<[u8; 20]>,
    /// The combined raw+metadata sha1 (v4/v5 only).
    pub combined_sha1: Option<[u8; 20]>,
    /// md5 of raw data only (v1–v3).
    pub raw_md5: Option<[u8; 16]>,
    pub has_parent: bool,
}

impl ChdHeader {
    /// The digest a MAME `disk` claim would carry for this file. v1/v2
    /// predate sha1 in the format: they answer `None`, which is why
    /// they can be *parsed* and reported without ever being linkable
    /// to a modern dat.
    #[must_use]
    pub const fn declared_disk_sha1(&self) -> Option<[u8; 20]> {
        match self.version {
            // v3's `sha1` field covers the raw data only.
            3 => self.raw_sha1,
            4 | 5 => self.combined_sha1,
            _ => None,
        }
    }

    /// Whether this CHD depends on a parent (delta dump).
    #[must_use]
    pub const fn has_parent(&self) -> bool {
        self.has_parent
    }

    /// True when no hunk can be compressed — v5 spells that as four
    /// `CODEC_NONE` slots, and stores an uncompressed map for it.
    #[must_use]
    pub fn is_uncompressed(&self) -> bool {
        self.compressors.iter().all(|&c| c == CODEC_NONE)
    }
}

/// Parse a CHD header from a file's first bytes.
///
/// `None` means "not a CHD at all" (no magic) — the caller's cue to try
/// another container. A file whose *magic* is present but whose header
/// is unreadable is an `Err`: it IS a CHD, and saying so beats
/// pretending the bytes are something else (D81 — a conclusion about
/// the bytes, not an environmental failure).
///
/// # Errors
/// [`ChdError::Malformed`] for a truncated header, an unknown version,
/// or a self-inconsistent geometry.
#[must_use]
pub fn try_parse_header(prefix: &[u8]) -> Option<Result<ChdHeader, ChdError>> {
    if !prefix.starts_with(CHD_MAGIC.as_slice()) {
        return None;
    }
    Some(parse_checked(prefix))
}

/// Header parse for callers that already know the bytes are a CHD.
///
/// # Errors
/// As [`try_parse_header`], plus a missing magic.
pub fn parse_header(prefix: &[u8]) -> Result<ChdHeader, ChdError> {
    match try_parse_header(prefix) {
        Some(r) => r,
        None => Err(ChdError::Malformed("not a CHD (no MComprHD magic)".into())),
    }
}

#[allow(clippy::too_many_lines)]
fn parse_checked(prefix: &[u8]) -> Result<ChdHeader, ChdError> {
    let trunc = || ChdError::Malformed("CHD header truncated".into());
    let be32 = |at: usize| -> Result<u32, ChdError> {
        prefix
            .get(at..at + 4)
            .map(|s| u32::from_be_bytes(s.try_into().expect("4 bytes")))
            .ok_or_else(trunc)
    };
    let be64 = |at: usize| -> Result<u64, ChdError> {
        prefix
            .get(at..at + 8)
            .map(|s| u64::from_be_bytes(s.try_into().expect("8 bytes")))
            .ok_or_else(trunc)
    };
    let sha1 = |at: usize| -> Result<[u8; 20], ChdError> {
        prefix
            .get(at..at + 20)
            .map(|s| <[u8; 20]>::try_from(s).expect("20 bytes"))
            .ok_or_else(trunc)
    };
    let md5 = |at: usize| -> Result<[u8; 16], ChdError> {
        prefix
            .get(at..at + 16)
            .map(|s| <[u8; 16]>::try_from(s).expect("16 bytes"))
            .ok_or_else(trunc)
    };

    let declared_len = be32(8)?;
    let version = be32(12)?;
    let want = match version {
        1 => CHD_V1_HEADER_LEN,
        2 => CHD_V2_HEADER_LEN,
        3 => CHD_V3_HEADER_LEN,
        4 => CHD_V4_HEADER_LEN,
        5 => CHD_V5_HEADER_LEN,
        v => return Err(ChdError::Malformed(format!("unknown CHD version {v}"))),
    };
    if prefix.len() < want {
        return Err(ChdError::Malformed(format!(
            "CHD v{version} header needs {want} bytes, got {}",
            prefix.len()
        )));
    }
    // MAME writes the exact header length here. A mismatch means the
    // file disagrees with itself about its own layout: refuse rather
    // than parse the fields we hope are there.
    if declared_len as usize != want {
        return Err(ChdError::Malformed(format!(
            "CHD v{version} declares a {declared_len}-byte header, not {want}"
        )));
    }

    if version == 5 {
        let hunk_bytes = be32(56)?;
        let logical_bytes = be64(32)?;
        let mut compressors = [0u32; 4];
        for (i, slot) in compressors.iter_mut().enumerate() {
            *slot = be32(16 + i * 4)?;
        }
        let parent = sha1(104)?;
        return Ok(ChdHeader {
            version,
            logical_bytes,
            hunk_bytes,
            unit_bytes: be32(60)?,
            total_hunks: hunk_count(logical_bytes, hunk_bytes)?,
            map_offset: be64(40)?,
            meta_offset: be64(48)?,
            compressors,
            raw_sha1: Some(sha1(64)?),
            combined_sha1: Some(sha1(84)?),
            raw_md5: None,
            has_parent: parent != [0u8; 20],
        });
    }

    let flags = be32(16)?;
    let compression = be32(20)?;
    let codec = legacy_codec(compression).ok_or_else(|| {
        ChdError::Malformed(format!(
            "CHD v{version} declares unknown compression {compression}"
        ))
    })?;
    let has_parent = flags & CHDFLAGS_HAS_PARENT != 0;

    let legacy = if version <= 2 {
        // Geometry-derived: cylinders * heads * sectors * seclen.
        let sec_len = if version == 1 {
            CHD_V1_SECTOR_SIZE
        } else {
            be32(76)?
        };
        let hunk_bytes = be32(24)?
            .checked_mul(sec_len)
            .ok_or_else(|| ChdError::Malformed("CHD v1/v2 hunk size overflows".into()))?;
        let logical =
            u64::from(be32(32)?) * u64::from(be32(36)?) * u64::from(be32(40)?) * u64::from(sec_len);
        Legacy {
            logical_bytes: logical,
            hunk_bytes,
            total_hunks: be32(28)?,
            meta_offset: 0,
            raw_md5: Some(md5(44)?),
            raw_sha1: None,
            combined_sha1: None,
        }
    } else if version == 3 {
        Legacy {
            logical_bytes: be64(28)?,
            hunk_bytes: be32(76)?,
            total_hunks: be32(24)?,
            meta_offset: be64(36)?,
            raw_md5: Some(md5(44)?),
            // v3's `sha1` field is the RAW data hash; the version has
            // no combined digest at all.
            raw_sha1: Some(sha1(80)?),
            combined_sha1: None,
        }
    } else {
        Legacy {
            logical_bytes: be64(28)?,
            hunk_bytes: be32(44)?,
            total_hunks: be32(24)?,
            meta_offset: be64(36)?,
            raw_md5: None,
            raw_sha1: Some(sha1(88)?),
            combined_sha1: Some(sha1(48)?),
        }
    };

    // v1–v4 store the hunk count explicitly. It must cover the logical
    // size, or the map cannot describe the data it claims to.
    let need = hunk_count(legacy.logical_bytes, legacy.hunk_bytes)?;
    if legacy.total_hunks < need {
        return Err(ChdError::Malformed(format!(
            "CHD v{version} declares {} hunks but needs {need} for {} logical bytes",
            legacy.total_hunks, legacy.logical_bytes
        )));
    }

    Ok(ChdHeader {
        version,
        logical_bytes: legacy.logical_bytes,
        hunk_bytes: legacy.hunk_bytes,
        unit_bytes: legacy.hunk_bytes,
        total_hunks: legacy.total_hunks,
        map_offset: want as u64,
        meta_offset: legacy.meta_offset,
        compressors: [codec, CODEC_NONE, CODEC_NONE, CODEC_NONE],
        raw_sha1: legacy.raw_sha1,
        combined_sha1: legacy.combined_sha1,
        raw_md5: legacy.raw_md5,
        has_parent,
    })
}

/// The version-dependent half of a v1–v4 header, so the shared checks
/// below it read as one block instead of a seven-element tuple.
struct Legacy {
    logical_bytes: u64,
    hunk_bytes: u32,
    total_hunks: u32,
    meta_offset: u64,
    raw_md5: Option<[u8; 16]>,
    raw_sha1: Option<[u8; 20]>,
    combined_sha1: Option<[u8; 20]>,
}

pub(crate) fn hunk_count(logical_bytes: u64, hunk_bytes: u32) -> Result<u32, ChdError> {
    if hunk_bytes == 0 {
        return Err(ChdError::Malformed("CHD hunk size is zero".into()));
    }
    let hunks = logical_bytes.div_ceil(u64::from(hunk_bytes));
    u32::try_from(hunks).map_err(|_| ChdError::Malformed("CHD hunk count overflows u32".into()))
}

/// How one hunk's bytes are obtained. Version-independent: each map
/// parser translates its own layout's flags into this vocabulary, so
/// the reader has exactly one shape to walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkKind {
    /// Compressed with this codec; `length` bytes live at `offset`.
    Codec(u32),
    /// `hunk_bytes` raw bytes at `offset`.
    Uncompressed,
    /// v1–v4: the whole hunk is the 8-byte `offset` field repeated.
    Mini,
    /// The same bytes as another hunk in this file (`offset` is its index).
    SelfHunk,
    /// Lives in the parent CHD (`offset` is a unit index there).
    ParentHunk,
    /// v5 uncompressed maps spell "never written" as offset zero; with
    /// no parent to fall back on, the hunk reads as zeros.
    Zeroed,
    /// The map says nothing usable about this hunk.
    Invalid,
}

/// One hunk map entry, normalised across versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HunkEntry {
    pub kind: HunkKind,
    pub offset: u64,
    pub length: u32,
    /// v3/v4 carry a crc32 of the *decompressed* hunk; v5 a crc16.
    /// v1/v2 carry neither — the format had no per-hunk checksum.
    pub crc32: Option<u32>,
    pub crc16: Option<u16>,
}

/// Decode a v1/v2 packed 8-byte map entry: 44-bit offset, 20-bit
/// length, and a *derived* type — the old format had no type field, so
/// "the stored run is exactly one hunk long" means uncompressed.
pub(crate) fn parse_legacy_entry(raw: &[u8; 8], hunk_bytes: u32, codec: u32) -> HunkEntry {
    let packed = u64::from_be_bytes(*raw);
    let offset = packed & 0x0000_0FFF_FFFF_FFFF;
    let length = u32::try_from(packed >> 44).expect("20 bits fit a u32");
    let kind = if length == hunk_bytes {
        HunkKind::Uncompressed
    } else {
        HunkKind::Codec(codec)
    };
    HunkEntry {
        kind,
        offset,
        length,
        crc32: None,
        crc16: None,
    }
}

/// Decode a v3/v4 16-byte map entry.
pub(crate) fn parse_v34_entry(raw: &[u8; 16], codec: u32) -> HunkEntry {
    let offset = u64::from_be_bytes(raw[0..8].try_into().expect("8 bytes"));
    let crc32 = u32::from_be_bytes(raw[8..12].try_into().expect("4 bytes"));
    let length = u32::from(u16::from_be_bytes(raw[12..14].try_into().expect("2 bytes")))
        | (u32::from(raw[14]) << 16);
    // Low nibble is the type; bit 4 says the crc field is meaningless.
    let has_crc = raw[15] & 0x10 == 0;
    let kind = match raw[15] & 0x0f {
        1 => HunkKind::Codec(codec),
        2 => HunkKind::Uncompressed,
        3 => HunkKind::Mini,
        4 => HunkKind::SelfHunk,
        5 => HunkKind::ParentHunk,
        // Type 6 is the "secondary algorithm" slot, only ever written
        // by the AV compressor — which we refuse wholesale.
        6 => HunkKind::Codec(CODEC_AVHUFF),
        _ => HunkKind::Invalid,
    };
    HunkEntry {
        kind,
        offset,
        length,
        crc32: has_crc.then_some(crc32),
        crc16: None,
    }
}
