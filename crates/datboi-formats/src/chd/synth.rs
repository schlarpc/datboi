//! Synthesised CHDs, for tests and fixtures.
//!
//! Real disc images cannot be committed to this repo, so the read path
//! is proved against CHDs this module writes: one per version, one per
//! codec, with correct maps, metadata chains and declared digests. A
//! round-trip through [`super::verify`] is then a real test of the
//! parser and every codec, with nothing copyrighted on disk.
//!
//! This is deliberately NOT a chdman port. It writes the simplest
//! legal encoding of each structure (no parents, no lossy codecs, no
//! clever hunk ordering); what it must get exactly right is the
//! *format*, so a reader bug cannot hide behind a writer that shares
//! it. Where a structure has an escape hatch the reader must handle —
//! the v5 map's run-length codes, a hunk that compresses worse than it
//! stores — the writer uses it.

use std::io::Write as _;

use super::codec;
use super::header::{
    CHD_MAGIC, CHD_V5_HEADER_LEN, CODEC_CD_FLAC, CODEC_CD_LZMA, CODEC_CD_ZLIB, CODEC_FLAC,
    CODEC_HUFFMAN, CODEC_LZMA, CODEC_NONE, CODEC_ZLIB,
};
use super::huffman::{BitWriter, Huffman, crc16};

/// What to synthesise. `version` picks the header layout; `codec` is
/// what compressed hunks use (v1–v4 only understand `zlib` or `none`).
#[derive(Debug, Clone)]
pub struct SynthSpec {
    pub version: u32,
    pub hunk_bytes: u32,
    pub unit_bytes: u32,
    pub codec: u32,
    /// `(tag, flags, bytes)` — flags bit 0 folds the entry into the
    /// combined sha1 (v4/v5).
    pub metadata: Vec<(u32, u8, Vec<u8>)>,
}

impl SynthSpec {
    /// A plain hard-disk-shaped CHD: 4 KiB hunks, one metadata entry.
    #[must_use]
    pub fn new(version: u32, codec: u32) -> Self {
        Self {
            version,
            hunk_bytes: 4096,
            unit_bytes: 512,
            codec,
            metadata: vec![(
                u32::from_be_bytes(*b"GDDD"),
                1,
                b"CYLS:2,HEADS:2,SECS:2,BPS:512".to_vec(),
            )],
        }
    }

    /// A CD-shaped CHD: hunks are whole 2448-byte frames, which is what
    /// the `cd*` codecs require.
    #[must_use]
    pub fn cd(codec: u32) -> Self {
        Self {
            version: 5,
            hunk_bytes: u32::try_from(codec::CD_FRAME_SIZE * 8).expect("small"),
            unit_bytes: u32::try_from(codec::CD_FRAME_SIZE).expect("small"),
            codec,
            metadata: vec![(
                u32::from_be_bytes(*b"CHT2"),
                1,
                b"TRACK:1 TYPE:MODE1_RAW SUBTYPE:NONE FRAMES:8 PREGAP:0 PGTYPE:MODE1 \
                  PGSUB:NONE POSTGAP:0"
                    .to_vec(),
            )],
        }
    }
}

/// Build a complete, valid CHD containing exactly `data`.
///
/// # Panics
/// On a spec the writer cannot honour (a codec a version predates, a
/// CD codec with a hunk size that is not whole frames) — a test-only
/// entry point, so a bad spec is a bug in the test, not input.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build(spec: &SynthSpec, data: &[u8]) -> Vec<u8> {
    use md5::Digest as _;

    assert!(spec.hunk_bytes > 0, "hunk size must be positive");
    if matches!(spec.codec, CODEC_CD_ZLIB | CODEC_CD_LZMA | CODEC_CD_FLAC) {
        assert_eq!(
            spec.hunk_bytes as usize % codec::CD_FRAME_SIZE,
            0,
            "CD codecs need whole-frame hunks"
        );
    }
    if spec.version < 5 {
        assert!(
            matches!(spec.codec, CODEC_NONE | CODEC_ZLIB),
            "CHD v1-v4 only ever carried zlib"
        );
    }

    let hunk_bytes = spec.hunk_bytes as usize;
    let logical = data.len() as u64;
    let hunks = data.len().div_ceil(hunk_bytes);

    // Compress every hunk up front: the map needs the lengths, and
    // whether each hunk beat storing it raw.
    let mut blocks: Vec<Block> = Vec::with_capacity(hunks);
    for i in 0..hunks {
        let start = i * hunk_bytes;
        let mut hunk = vec![0u8; hunk_bytes];
        let end = data.len().min(start + hunk_bytes);
        hunk[..end - start].copy_from_slice(&data[start..end]);
        let packed = compress(spec.codec, &hunk, spec.hunk_bytes);
        blocks.push(match packed {
            Some(bytes) if bytes.len() < hunk_bytes => Block::Compressed(bytes, hunk),
            _ => Block::Raw(hunk),
        });
    }

    let header_len = header_len(spec.version);
    let mut out = vec![0u8; header_len];

    // Body layout: map, then hunk payloads, then the metadata chain.
    // (chdman interleaves; a reader that cares would be wrong.)
    let map_offset = header_len as u64;
    let flat_v5 = spec.version == 5 && spec.codec == CODEC_NONE;
    let map_len = match spec.version {
        1 | 2 => hunks * 8,
        3 | 4 => hunks * 16,
        _ if flat_v5 => hunks * 4,
        _ => v5_map(spec, &blocks, 0, hunks).len(),
    };
    let mut payload_at = map_offset + map_len as u64;
    // A v5 flat map addresses hunks by index, so the payloads have to
    // start on a hunk boundary; pad up to one.
    let pad = if flat_v5 {
        let aligned = payload_at.div_ceil(u64::from(spec.hunk_bytes)) * u64::from(spec.hunk_bytes);
        let pad = aligned - payload_at;
        payload_at = aligned;
        usize::try_from(pad).expect("pad < hunk size")
    } else {
        0
    };

    let mut payloads = vec![0u8; pad];
    let mut offsets = Vec::with_capacity(hunks);
    for block in &blocks {
        offsets.push(map_offset + map_len as u64 + payloads.len() as u64);
        match block {
            Block::Compressed(bytes, _) => payloads.extend_from_slice(bytes),
            Block::Raw(hunk) => payloads.extend_from_slice(hunk),
        }
    }

    match spec.version {
        1 | 2 => {
            for (block, &offset) in blocks.iter().zip(&offsets) {
                let len = match block {
                    Block::Compressed(b, _) => b.len(),
                    Block::Raw(h) => h.len(),
                };
                let packed = ((len as u64) << 44) | offset;
                out.extend_from_slice(&packed.to_be_bytes());
            }
        }
        3 | 4 => {
            for (block, &offset) in blocks.iter().zip(&offsets) {
                let (len, kind, plain) = match block {
                    Block::Compressed(b, plain) => (b.len(), 1u8, plain),
                    Block::Raw(h) => (h.len(), 2u8, h),
                };
                out.extend_from_slice(&offset.to_be_bytes());
                let mut crc = crc32fast::Hasher::new();
                crc.update(plain);
                out.extend_from_slice(&crc.finalize().to_be_bytes());
                out.extend_from_slice(&u16::try_from(len & 0xffff).expect("masked").to_be_bytes());
                out.push(u8::try_from(len >> 16).expect("24-bit length"));
                out.push(kind);
            }
        }
        _ if flat_v5 => {
            for &offset in &offsets {
                let unit = offset / u64::from(spec.hunk_bytes);
                out.extend_from_slice(&u32::try_from(unit).expect("hunk index").to_be_bytes());
            }
        }
        _ => {
            let map = v5_map(spec, &blocks, payload_at, hunks);
            assert_eq!(map.len(), map_len, "v5 map length is not stable");
            out.extend_from_slice(&map);
        }
    }
    assert_eq!(
        out.len() as u64,
        map_offset + map_len as u64,
        "map length drifted"
    );
    out.extend_from_slice(&payloads);

    // Metadata chain: each entry points at the next; the last points
    // at zero.
    let mut meta_offset = 0u64;
    if spec.version >= 3 && !spec.metadata.is_empty() {
        meta_offset = out.len() as u64;
        let mut at = meta_offset;
        for (i, (tag, flags, body)) in spec.metadata.iter().enumerate() {
            let next = if i + 1 == spec.metadata.len() {
                0
            } else {
                at + 16 + body.len() as u64
            };
            out.extend_from_slice(&tag.to_be_bytes());
            out.push(*flags);
            let len = body.len();
            out.push(u8::try_from(len >> 16).expect("24-bit metadata"));
            out.push(u8::try_from((len >> 8) & 0xff).expect("masked"));
            out.push(u8::try_from(len & 0xff).expect("masked"));
            out.extend_from_slice(&next.to_be_bytes());
            out.extend_from_slice(body);
            at = next;
        }
    }

    // Digests over exactly the logical data.
    let raw_sha1: [u8; 20] = sha1::Sha1::digest(data).into();
    let raw_md5: [u8; 16] = md5::Md5::digest(data).into();
    let combined = super::combine_metadata(
        raw_sha1,
        &spec
            .metadata
            .iter()
            .map(|(tag, flags, body)| super::MetaEntry {
                tag: *tag,
                flags: *flags,
                data: body.clone(),
            })
            .collect::<Vec<_>>(),
    );

    let head = &mut out[..header_len];
    head[..8].copy_from_slice(CHD_MAGIC);
    head[8..12].copy_from_slice(&u32::try_from(header_len).expect("small").to_be_bytes());
    head[12..16].copy_from_slice(&spec.version.to_be_bytes());
    match spec.version {
        1 | 2 => {
            let sec_len: u32 = if spec.version == 1 {
                512
            } else {
                spec.unit_bytes
            };
            assert_eq!(spec.hunk_bytes % sec_len, 0, "hunk must be whole sectors");
            // Geometry has to multiply out to the logical size exactly.
            let sectors = logical / u64::from(sec_len);
            assert_eq!(
                logical % u64::from(sec_len),
                0,
                "v1/v2 logical size must be whole sectors"
            );
            head[16..20].copy_from_slice(&0u32.to_be_bytes()); // flags
            head[20..24].copy_from_slice(&legacy_compression(spec.codec).to_be_bytes());
            head[24..28].copy_from_slice(&(spec.hunk_bytes / sec_len).to_be_bytes());
            head[28..32].copy_from_slice(&u32::try_from(hunks).expect("small").to_be_bytes());
            head[32..36].copy_from_slice(&u32::try_from(sectors).expect("small").to_be_bytes());
            head[36..40].copy_from_slice(&1u32.to_be_bytes());
            head[40..44].copy_from_slice(&1u32.to_be_bytes());
            head[44..60].copy_from_slice(&raw_md5);
            if spec.version == 2 {
                head[76..80].copy_from_slice(&sec_len.to_be_bytes());
            }
        }
        3 => {
            head[16..20].copy_from_slice(&0u32.to_be_bytes());
            head[20..24].copy_from_slice(&legacy_compression(spec.codec).to_be_bytes());
            head[24..28].copy_from_slice(&u32::try_from(hunks).expect("small").to_be_bytes());
            head[28..36].copy_from_slice(&logical.to_be_bytes());
            head[36..44].copy_from_slice(&meta_offset.to_be_bytes());
            head[44..60].copy_from_slice(&raw_md5);
            head[76..80].copy_from_slice(&spec.hunk_bytes.to_be_bytes());
            // v3's `sha1` field is the RAW hash, metadata excluded.
            head[80..100].copy_from_slice(&raw_sha1);
        }
        4 => {
            head[16..20].copy_from_slice(&0u32.to_be_bytes());
            head[20..24].copy_from_slice(&legacy_compression(spec.codec).to_be_bytes());
            head[24..28].copy_from_slice(&u32::try_from(hunks).expect("small").to_be_bytes());
            head[28..36].copy_from_slice(&logical.to_be_bytes());
            head[36..44].copy_from_slice(&meta_offset.to_be_bytes());
            head[44..48].copy_from_slice(&spec.hunk_bytes.to_be_bytes());
            head[48..68].copy_from_slice(&combined);
            head[88..108].copy_from_slice(&raw_sha1);
        }
        _ => {
            if spec.codec != CODEC_NONE {
                head[16..20].copy_from_slice(&spec.codec.to_be_bytes());
            }
            head[32..40].copy_from_slice(&logical.to_be_bytes());
            head[40..48].copy_from_slice(&map_offset.to_be_bytes());
            head[48..56].copy_from_slice(&meta_offset.to_be_bytes());
            head[56..60].copy_from_slice(&spec.hunk_bytes.to_be_bytes());
            head[60..64].copy_from_slice(&spec.unit_bytes.to_be_bytes());
            head[64..84].copy_from_slice(&raw_sha1);
            head[84..104].copy_from_slice(&combined);
        }
    }
    out
}

enum Block {
    /// Compressed payload plus the plaintext it came from (the v3/v4
    /// map stores a crc32 of the latter).
    Compressed(Vec<u8>, Vec<u8>),
    Raw(Vec<u8>),
}

const fn header_len(version: u32) -> usize {
    use super::header::{
        CHD_V1_HEADER_LEN, CHD_V2_HEADER_LEN, CHD_V3_HEADER_LEN, CHD_V4_HEADER_LEN,
    };
    match version {
        1 => CHD_V1_HEADER_LEN,
        2 => CHD_V2_HEADER_LEN,
        3 => CHD_V3_HEADER_LEN,
        4 => CHD_V4_HEADER_LEN,
        _ => CHD_V5_HEADER_LEN,
    }
}

const fn legacy_compression(codec: u32) -> u32 {
    if codec == CODEC_NONE { 0 } else { 1 }
}

/// Emit the v5 compressed map: a 16-byte header, a 16-symbol tree in
/// MAME's RLE export form, then per-hunk types, lengths and CRCs.
///
/// The tree is deliberately flat — all sixteen symbols at four bits, so
/// each code IS its symbol — which keeps the writer honest (it cannot
/// accidentally share a bug with the reader's canonical-code
/// assignment) while still exercising every other part of the path.
fn v5_map(spec: &SynthSpec, blocks: &[Block], first_offset: u64, hunks: usize) -> Vec<u8> {
    let mut kinds = Vec::with_capacity(hunks);
    let mut lengths = Vec::with_capacity(hunks);
    for block in blocks {
        match block {
            Block::Compressed(bytes, _) => {
                kinds.push(0u32); // compressor slot 0
                lengths.push(u32::try_from(bytes.len()).expect("hunk length"));
            }
            Block::Raw(_) => {
                kinds.push(4u32); // COMPRESSION_NONE
                lengths.push(spec.hunk_bytes);
            }
        }
    }
    let max_len = lengths.iter().copied().max().unwrap_or(0);
    let length_bits = (32 - max_len.leading_zeros()).max(1);

    let mut bits = BitWriter::new();
    // Tree export (RLE form, 4-bit values): sixteen literal 4s.
    for _ in 0..16 {
        bits.write(4, 4);
    }
    // Pass 1: compression types, using the small run-length escape
    // wherever three or more in a row repeat the previous symbol.
    let mut i = 0;
    let mut last: Option<u32> = None;
    while i < hunks {
        let kind = kinds[i];
        if last == Some(kind) {
            let mut run = 1;
            while i + run < hunks && kinds[i + run] == kind && run < 18 {
                run += 1;
            }
            if run >= 3 {
                bits.write(7, 4); // COMPRESSION_RLE_SMALL
                bits.write(u32::try_from(run - 3).expect("run <= 18"), 4);
                i += run;
                continue;
            }
        }
        bits.write(kind, 4);
        last = Some(kind);
        i += 1;
    }
    // Pass 2: lengths and CRCs, and the 12-byte raw map the header's
    // CRC covers.
    let mut raw = vec![0u8; hunks * 12];
    let mut offset = first_offset;
    for (index, block) in blocks.iter().enumerate() {
        let plain = match block {
            Block::Compressed(_, p) | Block::Raw(p) => p,
        };
        let crc = crc16(plain);
        // An uncompressed entry stores no length: it is a whole hunk
        // by definition, and the reader supplies that itself.
        if kinds[index] != 4 {
            bits.write(lengths[index], length_bits);
        }
        bits.write(u32::from(crc), 16);
        let slot = &mut raw[index * 12..(index + 1) * 12];
        slot[0] = u8::try_from(kinds[index]).expect("type < 16");
        slot[1..4].copy_from_slice(&lengths[index].to_be_bytes()[1..]);
        slot[4..10].copy_from_slice(&offset.to_be_bytes()[2..]);
        slot[10..12].copy_from_slice(&crc.to_be_bytes());
        offset += u64::from(lengths[index]);
    }
    let body = bits.finish();

    let mut out = Vec::with_capacity(16 + body.len());
    out.extend_from_slice(&u32::try_from(body.len()).expect("map size").to_be_bytes());
    out.extend_from_slice(&first_offset.to_be_bytes()[2..]);
    out.extend_from_slice(&crc16(&raw).to_be_bytes());
    out.push(u8::try_from(length_bits).expect("<= 32"));
    out.push(24); // selfbits — no self-references are written
    out.push(24); // parentbits — likewise
    out.push(0); // reserved
    out.extend_from_slice(&body);
    out
}

/// Compress one hunk the way the named codec expects, or `None` for
/// `none` (which is stored raw).
fn compress(codec: u32, hunk: &[u8], hunk_bytes: u32) -> Option<Vec<u8>> {
    match codec {
        CODEC_NONE => None,
        CODEC_ZLIB => Some(deflate(hunk)),
        CODEC_LZMA => Some(lzma(hunk)),
        CODEC_HUFFMAN => Some(huff(hunk)),
        CODEC_FLAC => {
            let mut out = vec![b'B'];
            out.extend_from_slice(&flac_frames(hunk, true));
            Some(out)
        }
        CODEC_CD_ZLIB | CODEC_CD_LZMA | CODEC_CD_FLAC => Some(cd(codec, hunk, hunk_bytes)),
        other => panic!("synth cannot write codec {other:#010x}"),
    }
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(data).expect("in-memory write");
    enc.finish().expect("in-memory finish")
}

/// Raw LZMA1 with chdman's properties: take a standard `.lzma` stream
/// and drop its 13-byte header, which is exactly what chdman's
/// encoder configuration produces.
fn lzma(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut input = data;
    lzma_rs::lzma_compress(&mut input, &mut out).expect("in-memory compress");
    out.drain(..13);
    out
}

/// MAME's `huff` codec: a real 256-symbol tree over the hunk's own byte
/// frequencies, with its code lengths exported through a second,
/// 24-symbol tree (the format's `export_tree_huffman` shape).
///
/// Falls back to [`huff_flat`] when either tree would exceed its
/// format's bit ceiling.
fn huff(data: &[u8]) -> Vec<u8> {
    let mut counts = [0u32; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    let Some(lengths) = code_lengths(&counts, 16) else {
        return huff_flat(data);
    };
    // The small tree codes "this symbol's length, plus one" — zero is
    // reserved for the run-length escape, which this writer does not
    // use (a literal length per symbol is legal, just larger).
    let mut small_counts = [0u32; 24];
    for &len in &lengths {
        small_counts[len as usize + 1] += 1;
    }
    let Some(small_lengths) = code_lengths(&small_counts, 6) else {
        return huff_flat(data);
    };
    let (Ok(small), Ok(main)) = (
        Huffman::from_lengths(&small_lengths, 6),
        Huffman::from_lengths(&lengths, 16),
    ) else {
        return huff_flat(data);
    };

    let mut bits = BitWriter::new();
    bits.write(u32::from(small_lengths[0]), 3);
    bits.write(0, 3); // start = 1
    let last_used = small_lengths
        .iter()
        .rposition(|&l| l != 0)
        .expect("a tree has at least one code");
    for &len in &small_lengths[1..=last_used] {
        bits.write(u32::from(len), 3);
    }
    if last_used + 1 < 24 {
        bits.write(7, 3); // the "everything after here is unused" escape
    }
    for &len in &lengths {
        small.encode_one(&mut bits, len as usize + 1);
    }
    for &b in data {
        main.encode_one(&mut bits, b as usize);
    }
    bits.finish()
}

/// The degenerate `huff` encoding: all 256 codes eight bits long, which
/// the canonical assignment turns into the identity map. It never
/// compresses, so it only ever appears as a fallback — but it pins the
/// canonical-code assignment independently of the decoder, because the
/// bytes it produces are the bytes it was given.
fn huff_flat(data: &[u8]) -> Vec<u8> {
    let mut bits = BitWriter::new();
    // Small tree: length 1 for symbols 0 and 9, nothing else.
    bits.write(1, 3); // symbol 0's length
    bits.write(0, 3); // start = 1
    for index in 1..=10u32 {
        match index {
            9 => bits.write(1, 3),
            10 => bits.write(7, 3), // the "stop" escape
            _ => bits.write(0, 3),
        }
    }
    // Symbol 9 => "code length 8"; then run-length the other 255.
    bits.write(1, 1); // small-tree code for symbol 9
    bits.write(0, 1); // small-tree code for symbol 0 => run
    bits.write(7, 3); // 7 + 2 == 9, the escape into a wider count
    bits.write(255 - 9, 8); // rlefullbits for 256 symbols is 8
    for &b in data {
        bits.write(u32::from(b), 8);
    }
    bits.finish()
}

/// Huffman code lengths for a histogram, or `None` if the tree is
/// deeper than the format allows (or too small to be a complete code,
/// which the canonical assignment would reject).
fn code_lengths(counts: &[u32], max_bits: u32) -> Option<Vec<u8>> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let n = counts.len();
    let used: Vec<usize> = (0..n).filter(|&i| counts[i] > 0).collect();
    if used.len() < 2 {
        return None;
    }
    let mut parent = vec![usize::MAX; 2 * n];
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = used
        .iter()
        .map(|&i| Reverse((u64::from(counts[i]), i)))
        .collect();
    let mut next = n;
    while heap.len() > 1 {
        let Reverse((wa, a)) = heap.pop().expect("two or more nodes");
        let Reverse((wb, b)) = heap.pop().expect("two or more nodes");
        parent[a] = next;
        parent[b] = next;
        heap.push(Reverse((wa + wb, next)));
        next += 1;
    }
    let mut lengths = vec![0u8; n];
    for &i in &used {
        let mut depth = 0u32;
        let mut at = parent[i];
        while at != usize::MAX {
            depth += 1;
            at = parent[at];
        }
        if depth > max_bits {
            return None;
        }
        lengths[i] = u8::try_from(depth).expect("depth <= max_bits");
    }
    Some(lengths)
}

/// The CD framing: de-interleave sector data from subcode, drop every
/// sector's sync header and ECC parity where they are recomputable,
/// and compress the two runs.
fn cd(codec: u32, hunk: &[u8], hunk_bytes: u32) -> Vec<u8> {
    let frames = hunk.len() / codec::CD_FRAME_SIZE;
    let mut sectors = vec![0u8; frames * codec::CD_MAX_SECTOR_DATA];
    let mut subcodes = vec![0u8; frames * codec::CD_MAX_SUBCODE_DATA];
    // `cdfl` is the *audio* variant: there are no data sectors to strip
    // parity from, so it carries neither the bitmap nor a base length.
    let strips_ecc = codec != CODEC_CD_FLAC;
    let mut ecc_map = vec![0u8; if strips_ecc { frames.div_ceil(8) } else { 0 }];
    for f in 0..frames {
        let src = &hunk[f * codec::CD_FRAME_SIZE..(f + 1) * codec::CD_FRAME_SIZE];
        let sector =
            &mut sectors[f * codec::CD_MAX_SECTOR_DATA..(f + 1) * codec::CD_MAX_SECTOR_DATA];
        sector.copy_from_slice(&src[..codec::CD_MAX_SECTOR_DATA]);
        subcodes[f * codec::CD_MAX_SUBCODE_DATA..(f + 1) * codec::CD_MAX_SUBCODE_DATA]
            .copy_from_slice(&src[codec::CD_MAX_SECTOR_DATA..]);
        // "Recomputable" means: regenerating the parity reproduces what
        // is there. Anything else stays literal.
        let fixed =
            <[u8; codec::CD_MAX_SECTOR_DATA]>::try_from(&sector[..]).expect("sector length");
        let mut probe = fixed;
        codec::ecc_generate(&mut probe);
        if strips_ecc
            && fixed[..12] == *b"\x00\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\x00"
            && probe == fixed
        {
            ecc_map[f / 8] |= 1 << (f % 8);
            sector[..12].fill(0);
            sector[2076..].fill(0);
        }
    }
    let base = match codec {
        CODEC_CD_ZLIB => deflate(&sectors),
        CODEC_CD_LZMA => lzma(&sectors),
        _ => flac_frames(&sectors, true),
    };
    let sub = deflate(&subcodes);

    let mut out = ecc_map;
    if strips_ecc {
        // The base stream's length, in as many bytes as the hunk size
        // needs — the reader derives the same width.
        let width = if hunk_bytes < 65536 { 2 } else { 3 };
        let len = base.len();
        for shift in (0..width).rev() {
            out.push(u8::try_from((len >> (8 * shift)) & 0xff).expect("masked"));
        }
    }
    out.extend_from_slice(&base);
    out.extend_from_slice(&sub);
    out
}

// ---- FLAC frames ----

/// Encode `data` as bare FLAC frames of 16-bit stereo samples, taken in
/// the given byte order. One channel is written VERBATIM and the other
/// through a fixed order-0 predictor with Rice-coded residuals, so a
/// single fixture exercises both subframe shapes and the residual
/// decoder.
fn flac_frames(data: &[u8], big_endian: bool) -> Vec<u8> {
    const BLOCK: usize = 588;

    assert_eq!(data.len() % 4, 0, "FLAC hunks are whole stereo samples");
    let total = data.len() / 4;
    let mut out = Vec::new();
    let mut frame_number = 0u32;
    let mut at = 0usize;
    while at < total {
        let block = BLOCK.min(total - at);
        let mut bits = BitWriter::new();
        bits.write(0x3ffe, 14); // sync
        bits.write(0, 1); // reserved
        bits.write(0, 1); // fixed block size => this is a frame number
        bits.write(7, 4); // block size given as 16 bits below
        bits.write(9, 4); // 44.1 kHz
        bits.write(1, 4); // two independent channels
        bits.write(4, 3); // 16 bits per sample
        bits.write(0, 1); // reserved
        assert!(frame_number < 0x80, "fixture frames stay in one UTF-8 byte");
        bits.write(frame_number, 8);
        bits.write(u32::try_from(block - 1).expect("block size"), 16);
        let crc8 = crc8_flac(bits.bytes());
        bits.write(u32::from(crc8), 8);

        let sample = |i: usize, ch: usize| -> i32 {
            let at = (at + i) * 4 + ch * 2;
            let raw = [data[at], data[at + 1]];
            i32::from(if big_endian {
                i16::from_be_bytes(raw)
            } else {
                i16::from_le_bytes(raw)
            })
        };

        // Channel 0: VERBATIM.
        bits.write(0, 1);
        bits.write(1, 6);
        bits.write(0, 1);
        for i in 0..block {
            bits.write(sample(i, 0) as u32 & 0xffff, 16);
        }
        // Channel 1: FIXED order 0, one Rice partition. The parameter
        // is picked from the block's own magnitudes, the way a real
        // encoder does — a fixed one would make some fixtures larger
        // than the data they encode, and the writer would then store
        // them raw and never exercise this path at all.
        let zigzag = |i: usize| -> u32 {
            let v = sample(i, 1);
            ((v << 1) ^ (v >> 31)) as u32
        };
        let mean = (0..block).map(|i| u64::from(zigzag(i))).sum::<u64>() / block.max(1) as u64;
        let param = (64 - mean.max(1).leading_zeros()).saturating_sub(1).min(14);
        bits.write(0, 1);
        bits.write(8, 6);
        bits.write(0, 1);
        bits.write(0, 2); // Rice, 4-bit parameters
        bits.write(0, 4); // partition order 0
        bits.write(param, 4);
        for i in 0..block {
            let z = zigzag(i);
            for _ in 0..(z >> param) {
                bits.write(0, 1);
            }
            bits.write(1, 1);
            bits.write(z & ((1 << param) - 1), param);
        }
        bits.align_to_byte();
        let crc16 = crc16_flac(bits.bytes());
        bits.write(u32::from(crc16), 16);
        out.extend_from_slice(&bits.finish());

        at += block;
        frame_number += 1;
    }
    out
}

fn crc8_flac(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn crc16_flac(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Build a synthetic v5 header with nothing behind it (tests that only
/// need a CHD-shaped head). Predates [`build`] and stays because the
/// CLI's D44 audit test wants exactly this: a real header over bytes
/// that are not the disk it names.
#[must_use]
pub fn synth_v5(logical_bytes: u64, raw_sha1: [u8; 20], sha1: [u8; 20]) -> Vec<u8> {
    let mut h = vec![0u8; CHD_V5_HEADER_LEN];
    h[..8].copy_from_slice(CHD_MAGIC);
    h[8..12].copy_from_slice(
        &u32::try_from(CHD_V5_HEADER_LEN)
            .expect("small")
            .to_be_bytes(),
    );
    h[12..16].copy_from_slice(&5u32.to_be_bytes());
    h[32..40].copy_from_slice(&logical_bytes.to_be_bytes());
    h[56..60].copy_from_slice(&4096u32.to_be_bytes());
    h[60..64].copy_from_slice(&512u32.to_be_bytes());
    h[64..84].copy_from_slice(&raw_sha1);
    h[84..104].copy_from_slice(&sha1);
    // parent stays zero: standalone.
    h
}
