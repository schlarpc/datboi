//! CHD (MAME Compressed Hunks of Data): headers, hunk maps, and a
//! decompressing verify.
//!
//! MAME dats identify disks by the CHD's *internal* sha1 — a hash of
//! the decompressed data (plus, from v4, its metadata), NOT of the file
//! bytes — so audit needs the header to connect a stored `.chd` to a
//! disk claim. D44 ruled that a header read alone grades `probable`,
//! because the digest a CHD carries is written by whatever produced the
//! file: it is a self-attestation, and a truncated CHD with an intact
//! header must not audit as have.
//!
//! [`verify`] is the follow-up D44 deferred. It decompresses every
//! hunk, hashes the logical data the file is supposed to contain, and
//! compares that against the declaration. What comes back is evidence
//! we computed, not evidence the file asserted.
//!
//! Failure is always a *conclusion about the bytes* (D81): a codec this
//! build does not implement is [`ChdError::Unsupported`], a short or
//! self-contradictory file is [`ChdError::Malformed`], and only real
//! I/O trouble is [`ChdError::Io`]. Nothing here ever half-verifies —
//! a CHD whose 900th hunk needs `avhu` is refused with that reason,
//! never reported on the strength of the first 899.

mod codec;
mod flac;
mod header;
mod huffman;
pub mod synth;

use std::io::{Read, Seek, SeekFrom};

pub use codec::supported as codec_supported;
pub use header::{
    CHD_MAGIC, CHD_V5_HEADER_LEN, CODEC_AVHUFF, CODEC_CD_FLAC, CODEC_CD_LZMA, CODEC_CD_ZLIB,
    CODEC_FLAC, CODEC_HUFFMAN, CODEC_LZMA, CODEC_NONE, CODEC_ZLIB, ChdHeader, HunkEntry, HunkKind,
    codec_name, fourcc, parse_header, try_parse_header,
};
pub use synth::synth_v5;

/// Metadata's `flags` bit 0: this entry participates in the combined
/// (raw + metadata) sha1 that v4/v5 declare.
const CHD_MDFLAGS_CHECKSUM: u8 = 0x01;

/// A metadata chain entry can't sanely be longer than this; a longer
/// chain is a corrupt or hostile file, not a real dump.
const MAX_METADATA_ENTRIES: usize = 4096;

/// How deep a `SELF_HUNK` reference chain may go before we call the map
/// circular.
const MAX_SELF_DEPTH: u32 = 8;

/// What went wrong reading a CHD.
///
/// The split is load-bearing for the refinement fixpoint (D81):
/// `Malformed` and `Unsupported` are settled conclusions about these
/// bytes and must never be retried; `Io` is environmental and may
/// succeed later.
#[derive(Debug)]
pub enum ChdError {
    /// The file contradicts the CHD format, or itself.
    Malformed(String),
    /// A real, well-formed CHD this build cannot decode.
    Unsupported(String),
    /// The byte source failed.
    Io(std::io::Error),
}

impl std::fmt::Display for ChdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(m) | Self::Unsupported(m) => f.write_str(m),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ChdError {}

impl ChdError {
    /// Whether this is a settled conclusion about the bytes rather than
    /// an environmental failure — the D81 question every caller asks.
    #[must_use]
    pub const fn is_conclusion(&self) -> bool {
        matches!(self, Self::Malformed(_) | Self::Unsupported(_))
    }
}

/// One metadata entry from the v3+ chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaEntry {
    pub tag: u32,
    pub flags: u8,
    pub data: Vec<u8>,
}

/// A CHD opened for reading: header, full hunk map, metadata chain.
pub struct ChdReader<R> {
    src: R,
    header: ChdHeader,
    map: Vec<HunkEntry>,
    metadata: Vec<MetaEntry>,
    decoder: codec::Decoder,
    compressed: Vec<u8>,
}

impl<R: Read + Seek> ChdReader<R> {
    /// Parse a CHD's header, hunk map and metadata chain.
    ///
    /// # Errors
    /// [`ChdError::Malformed`] for anything the file contradicts,
    /// [`ChdError::Io`] for a failing byte source.
    pub fn open(mut src: R) -> Result<Self, ChdError> {
        let mut head = [0u8; CHD_V5_HEADER_LEN];
        let got = read_at(&mut src, 0, &mut head)?;
        let header = parse_header(&head[..got])?;
        let map = read_map(&mut src, &header)?;
        let metadata = read_metadata(&mut src, &header)?;
        let decoder = codec::Decoder::new(header.hunk_bytes);
        Ok(Self {
            src,
            header,
            map,
            metadata,
            decoder,
            compressed: Vec::new(),
        })
    }

    #[must_use]
    pub const fn header(&self) -> &ChdHeader {
        &self.header
    }

    #[must_use]
    pub fn metadata(&self) -> &[MetaEntry] {
        &self.metadata
    }

    #[must_use]
    pub fn map(&self) -> &[HunkEntry] {
        &self.map
    }

    /// The first codec the map actually uses that this build cannot
    /// decode, if any. Answering from the *map* rather than the header
    /// matters: a CHD may declare `cdfl` in a compressor slot and never
    /// choose it for a single hunk.
    #[must_use]
    pub fn unsupported_codec(&self) -> Option<u32> {
        self.map.iter().find_map(|e| match e.kind {
            HunkKind::Codec(c) if !codec::supported(c) => Some(c),
            _ => None,
        })
    }

    /// Decode hunk `index` into `dest`, which must be `hunk_bytes` long.
    ///
    /// # Errors
    /// As the module docs: refusals and corruption are conclusions,
    /// only the byte source raises [`ChdError::Io`].
    pub fn read_hunk(&mut self, index: u32, dest: &mut [u8]) -> Result<(), ChdError> {
        self.read_hunk_depth(index, dest, 0)
    }

    fn read_hunk_depth(&mut self, index: u32, dest: &mut [u8], depth: u32) -> Result<(), ChdError> {
        if depth > MAX_SELF_DEPTH {
            return Err(ChdError::Malformed(
                "CHD hunk map self-references in a cycle".into(),
            ));
        }
        let entry = *self.map.get(index as usize).ok_or_else(|| {
            ChdError::Malformed(format!("CHD hunk {index} is past the end of the map"))
        })?;
        match entry.kind {
            HunkKind::Zeroed => {
                dest.fill(0);
                Ok(())
            }
            HunkKind::Mini => {
                // The 8-byte offset field, tiled across the hunk.
                let pattern = entry.offset.to_be_bytes();
                for (i, slot) in dest.iter_mut().enumerate() {
                    *slot = pattern[i % 8];
                }
                Ok(())
            }
            HunkKind::SelfHunk => {
                let target = u32::try_from(entry.offset).map_err(|_| {
                    ChdError::Malformed("CHD self-reference index overflows".into())
                })?;
                if target == index {
                    return Err(ChdError::Malformed(format!(
                        "CHD hunk {index} references itself"
                    )));
                }
                self.read_hunk_depth(target, dest, depth + 1)
            }
            HunkKind::ParentHunk => Err(ChdError::Unsupported(
                "CHD hunk lives in a parent file; delta CHDs cannot be verified standalone".into(),
            )),
            HunkKind::Invalid => Err(ChdError::Malformed(format!(
                "CHD hunk {index} has no valid map entry"
            ))),
            // The two kinds whose bytes come off disk. Both then face
            // the map's own checksum below; an uncompressed hunk is no
            // less capable of being corrupt than a compressed one.
            HunkKind::Uncompressed => {
                let got = read_at(&mut self.src, entry.offset, dest)?;
                if got != dest.len() {
                    return Err(ChdError::Malformed(format!(
                        "CHD hunk {index} runs {} bytes past the end of the file",
                        dest.len() - got
                    )));
                }
                self.check_hunk(index, &entry, dest)
            }
            HunkKind::Codec(c) => {
                self.compressed.clear();
                self.compressed.resize(entry.length as usize, 0);
                let got = read_at(&mut self.src, entry.offset, &mut self.compressed)?;
                if got != self.compressed.len() {
                    return Err(ChdError::Malformed(format!(
                        "CHD hunk {index} runs {} bytes past the end of the file",
                        self.compressed.len() - got
                    )));
                }
                self.decoder.decode(c, &self.compressed, dest)?;
                self.check_hunk(index, &entry, dest)
            }
        }
    }

    /// The map's per-hunk checksum. Written by the same tool as the
    /// file's declared sha1, so it is NOT evidence of identity (D44's
    /// whole point) — but it localises corruption to one hunk instead
    /// of leaving a whole-file digest mismatch with nothing to say.
    fn check_hunk(&self, index: u32, entry: &HunkEntry, dest: &[u8]) -> Result<(), ChdError> {
        if let Some(want) = entry.crc32 {
            let got = crc32(dest);
            if got != want {
                return Err(ChdError::Malformed(format!(
                    "CHD hunk {index} is crc32 {got:#010x}, the map says {want:#010x}"
                )));
            }
        }
        if let Some(want) = entry.crc16 {
            let got = huffman::crc16(dest);
            if got != want {
                return Err(ChdError::Malformed(format!(
                    "CHD hunk {index} is crc16 {got:#06x}, the map says {want:#06x}"
                )));
            }
        }
        Ok(())
    }
}

/// What a decompressing verify concluded about a CHD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub header: ChdHeader,
    /// sha1 over exactly `logical_bytes` of decompressed data.
    pub raw_sha1: [u8; 20],
    /// The combined raw+metadata digest, computed MAME's way (v4/v5).
    pub combined_sha1: Option<[u8; 20]>,
    /// md5 over the raw data — the only digest v1/v2 declare.
    pub raw_md5: [u8; 16],
    /// Whether every digest the header declares matched what we
    /// computed. `false` means the file lied about its own contents.
    pub declaration_holds: bool,
    /// Which declared digest disagreed, for the verdict's detail.
    pub mismatch: Option<String>,
}

/// Decompress a whole CHD and hash what comes out.
///
/// `progress` is called with the number of logical bytes produced so
/// far, so a long walk can keep an analyzer's lease alive (D71).
///
/// # Errors
/// [`ChdError::Unsupported`] when any hunk needs a codec this build
/// refuses (named), [`ChdError::Malformed`] for a short or
/// self-contradictory file, [`ChdError::Io`] for the byte source.
pub fn verify<R: Read + Seek>(
    src: R,
    progress: &mut dyn FnMut(u64),
) -> Result<Verification, ChdError> {
    use md5::Digest as _;

    let mut reader = ChdReader::open(src)?;
    // Refuse up front, before spending minutes on a file we cannot
    // finish: a partly-decompressed CHD is not evidence of anything.
    if let Some(c) = reader.unsupported_codec() {
        return Err(ChdError::Unsupported(format!(
            "CHD uses codec `{}`, which this build cannot decode",
            codec_name(c)
        )));
    }
    if reader.header.has_parent() {
        return Err(ChdError::Unsupported(
            "delta CHD: its data lives in a parent file, so it cannot be verified standalone"
                .into(),
        ));
    }

    let header = reader.header.clone();
    let hunk_bytes = header.hunk_bytes as usize;
    let mut hunk = vec![0u8; hunk_bytes];
    let mut sha = sha1::Sha1::new();
    let mut md5 = md5::Md5::new();
    let mut remaining = header.logical_bytes;
    let mut index = 0u32;
    while remaining > 0 {
        reader.read_hunk(index, &mut hunk)?;
        // The final hunk is padded; the declared digests cover exactly
        // `logical_bytes`, never the padding.
        let take = usize::try_from(remaining.min(hunk_bytes as u64)).expect("min with usize");
        sha.update(&hunk[..take]);
        md5.update(&hunk[..take]);
        remaining -= take as u64;
        index += 1;
        progress(header.logical_bytes - remaining);
    }
    let raw_sha1: [u8; 20] = sha.finalize().into();
    let raw_md5: [u8; 16] = md5.finalize().into();

    // v4 and v5 declare a digest over the raw data *plus* the checksummed
    // metadata entries, folded in MAME's order (see `combine_metadata`).
    let combined_sha1 =
        (header.version >= 4).then(|| combine_metadata(raw_sha1, reader.metadata()));

    let mut mismatch = None;
    if let Some(declared) = header.raw_sha1
        && header.version >= 4
        && declared != raw_sha1
    {
        mismatch = Some(format!(
            "declared rawsha1 {} but the data hashes to {}",
            hex(&declared),
            hex(&raw_sha1)
        ));
    }
    if mismatch.is_none()
        && header.version == 3
        && let Some(declared) = header.raw_sha1
        && declared != raw_sha1
    {
        mismatch = Some(format!(
            "declared sha1 {} but the data hashes to {}",
            hex(&declared),
            hex(&raw_sha1)
        ));
    }
    if mismatch.is_none()
        && let (Some(declared), Some(got)) = (header.combined_sha1, combined_sha1)
        && declared != got
    {
        mismatch = Some(format!(
            "declared sha1 {} but data+metadata hash to {}",
            hex(&declared),
            hex(&got)
        ));
    }
    if mismatch.is_none()
        && let Some(declared) = header.raw_md5
        && declared != raw_md5
    {
        mismatch = Some(format!(
            "declared md5 {} but the data hashes to {}",
            hex(&declared),
            hex(&raw_md5)
        ));
    }

    Ok(Verification {
        header,
        raw_sha1,
        combined_sha1,
        raw_md5,
        declaration_holds: mismatch.is_none(),
        mismatch,
    })
}

/// MAME's `compute_overall_sha1`: sha1 over the raw digest followed by
/// one 24-byte `(tag, sha1-of-entry)` record per checksummed metadata
/// entry, the records sorted by their own bytes. Metadata order in the
/// file is therefore not part of the identity — which is the whole
/// point, since chdman rewrites the chain freely.
fn combine_metadata(raw_sha1: [u8; 20], metadata: &[MetaEntry]) -> [u8; 20] {
    use sha1::Digest as _;

    let mut records: Vec<[u8; 24]> = metadata
        .iter()
        .filter(|m| m.flags & CHD_MDFLAGS_CHECKSUM != 0)
        .map(|m| {
            let mut record = [0u8; 24];
            record[..4].copy_from_slice(&m.tag.to_be_bytes());
            let digest: [u8; 20] = sha1::Sha1::digest(&m.data).into();
            record[4..].copy_from_slice(&digest);
            record
        })
        .collect();
    records.sort_unstable();
    let mut hasher = sha1::Sha1::new();
    hasher.update(raw_sha1);
    for record in &records {
        hasher.update(record);
    }
    hasher.finalize().into()
}

fn read_map<R: Read + Seek>(src: &mut R, header: &ChdHeader) -> Result<Vec<HunkEntry>, ChdError> {
    let hunks = header.total_hunks as usize;
    if header.version == 5 {
        return if header.is_uncompressed() {
            read_v5_flat_map(src, header)
        } else {
            read_v5_compressed_map(src, header)
        };
    }
    let entry_size = if header.version <= 2 { 8 } else { 16 };
    let mut raw = vec![0u8; hunks * entry_size];
    let got = read_at(src, header.map_offset, &mut raw)?;
    if got != raw.len() {
        return Err(ChdError::Malformed(format!(
            "CHD v{} hunk map is truncated ({got} of {} bytes)",
            header.version,
            raw.len()
        )));
    }
    let codec = header.compressors[0];
    Ok(raw
        .chunks_exact(entry_size)
        .map(|chunk| {
            if entry_size == 8 {
                header::parse_legacy_entry(
                    chunk.try_into().expect("8 bytes"),
                    header.hunk_bytes,
                    codec,
                )
            } else {
                header::parse_v34_entry(chunk.try_into().expect("16 bytes"), codec)
            }
        })
        .collect())
}

/// A v5 CHD with every compressor slot `none` stores a flat map of
/// 32-bit hunk *indexes*: offset = value * `hunk_bytes`, and zero means
/// "never written".
fn read_v5_flat_map<R: Read + Seek>(
    src: &mut R,
    header: &ChdHeader,
) -> Result<Vec<HunkEntry>, ChdError> {
    let mut raw = vec![0u8; header.total_hunks as usize * 4];
    let got = read_at(src, header.map_offset, &mut raw)?;
    if got != raw.len() {
        return Err(ChdError::Malformed(
            "CHD v5 uncompressed hunk map is truncated".into(),
        ));
    }
    Ok(raw
        .chunks_exact(4)
        .map(|c| {
            let unit = u64::from(u32::from_be_bytes(c.try_into().expect("4 bytes")));
            HunkEntry {
                kind: if unit == 0 {
                    HunkKind::Zeroed
                } else {
                    HunkKind::Uncompressed
                },
                offset: unit * u64::from(header.hunk_bytes),
                length: header.hunk_bytes,
                crc32: None,
                crc16: None,
            }
        })
        .collect())
}

/// v5 compressed-map header, the 16 bytes before the bit stream.
const V5_MAP_HEADER_LEN: usize = 16;

/// The v5 map's own symbol alphabet. 0–3 index the header's compressor
/// slots; the rest are structural.
const COMPRESSION_NONE: u32 = 4;
const COMPRESSION_SELF: u32 = 5;
const COMPRESSION_PARENT: u32 = 6;
const COMPRESSION_RLE_SMALL: u32 = 7;
const COMPRESSION_RLE_LARGE: u32 = 8;
const COMPRESSION_SELF_0: u32 = 9;
const COMPRESSION_SELF_1: u32 = 10;
const COMPRESSION_PARENT_SELF: u32 = 11;
const COMPRESSION_PARENT_0: u32 = 12;
const COMPRESSION_PARENT_1: u32 = 13;

/// Decode the v5 hunk map: a 16-symbol huffman tree over per-hunk
/// compression types (with two RLE escapes), then bit-packed lengths,
/// CRCs and back-references. Offsets are implicit — each compressed
/// hunk follows the last — which is why the whole map must be walked
/// in order even to read one hunk.
#[allow(clippy::too_many_lines)]
fn read_v5_compressed_map<R: Read + Seek>(
    src: &mut R,
    header: &ChdHeader,
) -> Result<Vec<HunkEntry>, ChdError> {
    let mut head = [0u8; V5_MAP_HEADER_LEN];
    if read_at(src, header.map_offset, &mut head)? != V5_MAP_HEADER_LEN {
        return Err(ChdError::Malformed("CHD v5 map header is truncated".into()));
    }
    let map_bytes = u32::from_be_bytes(head[0..4].try_into().expect("4 bytes")) as usize;
    let first_offset = be48(&head[4..10]);
    let map_crc = u16::from_be_bytes(head[10..12].try_into().expect("2 bytes"));
    let length_bits = u32::from(head[12]);
    let self_bits = u32::from(head[13]);
    let parent_bits = u32::from(head[14]);
    if length_bits > 32 || self_bits > 32 || parent_bits > 32 {
        return Err(ChdError::Malformed(
            "CHD v5 map header declares an impossible field width".into(),
        ));
    }

    let mut compressed = vec![0u8; map_bytes];
    if read_at(
        src,
        header.map_offset + V5_MAP_HEADER_LEN as u64,
        &mut compressed,
    )? != map_bytes
    {
        return Err(ChdError::Malformed("CHD v5 map is truncated".into()));
    }

    let hunks = header.total_hunks as usize;
    let mut bits = huffman::BitReader::new(&compressed);
    let tree = huffman::Huffman::import_tree_rle(16, 8, &mut bits)?;

    // Pass 1: the per-hunk compression type, RLE-coded.
    let mut kinds = vec![0u32; hunks];
    let mut last = 0u32;
    let mut repeat = 0u32;
    for slot in &mut kinds {
        if repeat > 0 {
            *slot = last;
            repeat -= 1;
            continue;
        }
        let value = tree.decode_one(&mut bits);
        match value {
            COMPRESSION_RLE_SMALL => {
                *slot = last;
                repeat = 2 + tree.decode_one(&mut bits);
            }
            COMPRESSION_RLE_LARGE => {
                *slot = last;
                repeat = 2 + 16 + (tree.decode_one(&mut bits) << 4);
                repeat += tree.decode_one(&mut bits);
            }
            v => {
                last = v;
                *slot = v;
            }
        }
        if bits.overflowed() {
            return Err(ChdError::Malformed("CHD v5 map is truncated".into()));
        }
    }

    // Pass 2: lengths, CRCs and back-references, rebuilding MAME's
    // 12-byte raw map so the header's CRC can be checked against it.
    let mut raw = vec![0u8; hunks * 12];
    let mut cur_offset = first_offset;
    let mut last_self = 0u64;
    let mut last_parent = 0u64;
    let mut entries = Vec::with_capacity(hunks);
    for (index, &kind) in kinds.iter().enumerate() {
        let mut offset = cur_offset;
        let mut length = 0u32;
        let mut crc = 0u16;
        let mut stored_kind = kind;
        match kind {
            0..=3 => {
                length = bits.read(length_bits);
                cur_offset += u64::from(length);
                crc = u16::try_from(bits.read(16)).expect("16 bits");
            }
            COMPRESSION_NONE => {
                length = header.hunk_bytes;
                cur_offset += u64::from(length);
                crc = u16::try_from(bits.read(16)).expect("16 bits");
            }
            COMPRESSION_SELF => {
                offset = u64::from(bits.read(self_bits));
                last_self = offset;
            }
            COMPRESSION_PARENT => {
                offset = u64::from(bits.read(parent_bits));
                last_parent = offset;
            }
            COMPRESSION_SELF_0 | COMPRESSION_SELF_1 => {
                if kind == COMPRESSION_SELF_1 {
                    last_self += 1;
                }
                stored_kind = COMPRESSION_SELF;
                offset = last_self;
            }
            COMPRESSION_PARENT_SELF => {
                stored_kind = COMPRESSION_PARENT;
                last_parent = (index as u64 * u64::from(header.hunk_bytes))
                    / u64::from(header.unit_bytes.max(1));
                offset = last_parent;
            }
            COMPRESSION_PARENT_0 | COMPRESSION_PARENT_1 => {
                if kind == COMPRESSION_PARENT_1 {
                    last_parent += u64::from(header.hunk_bytes / header.unit_bytes.max(1));
                }
                stored_kind = COMPRESSION_PARENT;
                offset = last_parent;
            }
            other => {
                return Err(ChdError::Malformed(format!(
                    "CHD v5 map hunk {index} has compression type {other}"
                )));
            }
        }
        let slot = &mut raw[index * 12..(index + 1) * 12];
        slot[0] = u8::try_from(stored_kind).expect("type < 16");
        slot[1..4].copy_from_slice(&length.to_be_bytes()[1..]);
        slot[4..10].copy_from_slice(&offset.to_be_bytes()[2..]);
        slot[10..12].copy_from_slice(&crc.to_be_bytes());

        entries.push(HunkEntry {
            kind: match stored_kind {
                0..=3 => {
                    let c = header.compressors[stored_kind as usize];
                    HunkKind::Codec(c)
                }
                COMPRESSION_NONE => HunkKind::Uncompressed,
                COMPRESSION_SELF => HunkKind::SelfHunk,
                _ => HunkKind::ParentHunk,
            },
            offset,
            length,
            crc32: None,
            crc16: (stored_kind <= COMPRESSION_NONE).then_some(crc),
        });
    }
    if bits.overflowed() {
        return Err(ChdError::Malformed("CHD v5 map is truncated".into()));
    }
    let got = huffman::crc16(&raw);
    if got != map_crc {
        return Err(ChdError::Malformed(format!(
            "CHD v5 hunk map fails its own CRC ({got:#06x}, header says {map_crc:#06x})"
        )));
    }
    Ok(entries)
}

/// Walk the v3+ metadata chain. v1/v2 have none.
fn read_metadata<R: Read + Seek>(
    src: &mut R,
    header: &ChdHeader,
) -> Result<Vec<MetaEntry>, ChdError> {
    let mut out = Vec::new();
    let mut at = header.meta_offset;
    let mut seen = 0usize;
    while at != 0 {
        seen += 1;
        if seen > MAX_METADATA_ENTRIES {
            return Err(ChdError::Malformed(
                "CHD metadata chain does not terminate".into(),
            ));
        }
        let mut head = [0u8; 16];
        if read_at(src, at, &mut head)? != head.len() {
            return Err(ChdError::Malformed(
                "CHD metadata entry runs past the end of the file".into(),
            ));
        }
        let tag = u32::from_be_bytes(head[0..4].try_into().expect("4 bytes"));
        let flags = head[4];
        let length =
            (usize::from(head[5]) << 16) | (usize::from(head[6]) << 8) | usize::from(head[7]);
        let next = u64::from_be_bytes(head[8..16].try_into().expect("8 bytes"));
        let mut data = vec![0u8; length];
        if read_at(src, at + 16, &mut data)? != length {
            return Err(ChdError::Malformed(
                "CHD metadata entry runs past the end of the file".into(),
            ));
        }
        out.push(MetaEntry { tag, flags, data });
        if next == at {
            return Err(ChdError::Malformed(
                "CHD metadata chain points at itself".into(),
            ));
        }
        at = next;
    }
    Ok(out)
}

/// Read into `buf` at `offset`, answering how many bytes were actually
/// there. A short read is NOT an error here — the callers turn it into
/// a `Malformed` conclusion with the context to say what was short,
/// because "the file is shorter than it claims" is a fact about the
/// bytes, not an environmental failure (D81, and the D44 strictness
/// this whole module exists to deliver).
fn read_at<R: Read + Seek>(src: &mut R, offset: u64, buf: &mut [u8]) -> Result<usize, ChdError> {
    src.seek(SeekFrom::Start(offset)).map_err(ChdError::Io)?;
    let mut filled = 0;
    while filled < buf.len() {
        match src.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(ChdError::Io(e)),
        }
    }
    Ok(filled)
}

fn be48(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
}

fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests;
