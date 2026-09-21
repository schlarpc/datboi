//! CHD hunk codecs.
//!
//! A CHD hunk is compressed on its own, with a codec chosen per hunk
//! from the four the header declares. Decoding one is therefore a pure
//! `(codec, compressed bytes, exact output length) -> bytes` function —
//! which is what makes a decompressing verify possible at all.
//!
//! Coverage is deliberately explicit: [`supported`] answers for a codec
//! tag, and anything it refuses is named in the verdict rather than
//! skipped. A partly-decoded CHD must never look verified.
//!
//! The CD codecs (`cdzl`/`cdlz`/`cdfl`) are not codecs so much as a
//! framing: a CD hunk is N 2448-byte frames of 2352 sector bytes plus
//! 96 subcode bytes, and the compressor de-interleaves those into two
//! runs before handing each to an ordinary codec. It also *deletes*
//! every sector's sync header and Reed-Solomon parity when they are
//! recomputable, flagging that in a bitmap; we regenerate them here.

use super::header::{
    CODEC_CD_FLAC, CODEC_CD_LZMA, CODEC_CD_ZLIB, CODEC_FLAC, CODEC_HUFFMAN, CODEC_LZMA, CODEC_NONE,
    CODEC_ZLIB, codec_name,
};
use super::huffman::{BitReader, Huffman};
use super::{ChdError, flac};

/// One CD frame as a CHD stores it: the raw sector plus its subcode.
pub(crate) const CD_FRAME_SIZE: usize = 2448;
pub(crate) const CD_MAX_SECTOR_DATA: usize = 2352;
pub(crate) const CD_MAX_SUBCODE_DATA: usize = 96;

/// The 12-byte sync pattern every data sector starts with (ECMA-130).
const CD_SYNC_HEADER: [u8; 12] = [
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00,
];

/// Whether this build can decode the codec — the single source of
/// truth behind every "refused: codec X" verdict.
#[must_use]
pub fn supported(codec: u32) -> bool {
    matches!(
        codec,
        CODEC_NONE
            | CODEC_ZLIB
            | CODEC_LZMA
            | CODEC_HUFFMAN
            | CODEC_FLAC
            | CODEC_CD_ZLIB
            | CODEC_CD_LZMA
            | CODEC_CD_FLAC
    )
}

/// Hunk decoder. Holds the scratch the CD framing needs so a whole-file
/// walk allocates once, not per hunk.
pub(crate) struct Decoder {
    hunk_bytes: u32,
    /// De-interleaved sector run followed by subcode run.
    cd_scratch: Vec<u8>,
}

impl Decoder {
    pub(crate) fn new(hunk_bytes: u32) -> Self {
        Self {
            hunk_bytes,
            cd_scratch: Vec::new(),
        }
    }

    /// Decode one hunk into `dest`, which is exactly the hunk's
    /// decompressed length.
    ///
    /// # Errors
    /// [`ChdError::Unsupported`] for a codec this build refuses;
    /// [`ChdError::Malformed`] when the stream does not produce exactly
    /// `dest.len()` bytes. Both are conclusions about the bytes (D81).
    pub(crate) fn decode(
        &mut self,
        codec: u32,
        src: &[u8],
        dest: &mut [u8],
    ) -> Result<(), ChdError> {
        match codec {
            CODEC_NONE => {
                if src.len() != dest.len() {
                    return Err(ChdError::Malformed(format!(
                        "uncompressed hunk is {} bytes, not {}",
                        src.len(),
                        dest.len()
                    )));
                }
                dest.copy_from_slice(src);
                Ok(())
            }
            CODEC_ZLIB => inflate_raw(src, dest),
            CODEC_LZMA => lzma_raw(src, dest, self.hunk_bytes),
            CODEC_HUFFMAN => huffman_decode(src, dest),
            CODEC_FLAC => flac::decode_hunk(src, dest),
            CODEC_CD_ZLIB | CODEC_CD_LZMA | CODEC_CD_FLAC => self.decode_cd(codec, src, dest),
            other => Err(ChdError::Unsupported(format!(
                "CHD codec `{}` is not implemented",
                codec_name(other)
            ))),
        }
    }

    /// The CD framing shared by `cdzl`, `cdlz` and `cdfl`.
    fn decode_cd(&mut self, codec: u32, src: &[u8], dest: &mut [u8]) -> Result<(), ChdError> {
        if !dest.len().is_multiple_of(CD_FRAME_SIZE) {
            return Err(ChdError::Malformed(format!(
                "CD hunk of {} bytes is not a whole number of {CD_FRAME_SIZE}-byte frames",
                dest.len()
            )));
        }
        let frames = dest.len() / CD_FRAME_SIZE;
        let sector_run = frames * CD_MAX_SECTOR_DATA;
        let subcode_run = frames * CD_MAX_SUBCODE_DATA;
        self.cd_scratch.clear();
        self.cd_scratch.resize(sector_run + subcode_run, 0);
        let (sectors, subcodes) = self.cd_scratch.split_at_mut(sector_run);

        // `cdfl` has no ECC bitmap and no length prefix: the FLAC
        // stream is self-delimiting, and its own end marks where the
        // deflated subcode begins. The other two prefix an ECC bitmap
        // and the base stream's length.
        let ecc_bytes = if codec == CODEC_CD_FLAC {
            0
        } else {
            frames.div_ceil(8)
        };
        if codec == CODEC_CD_FLAC {
            let used = flac::decode_cd_sectors(src, sectors)?;
            let rest = src
                .get(used..)
                .ok_or_else(|| ChdError::Malformed("cdfl hunk is truncated".into()))?;
            inflate_raw(rest, subcodes)?;
        } else {
            let complen_bytes = if dest.len() < 65536 { 2 } else { 3 };
            let header_bytes = ecc_bytes + complen_bytes;
            if src.len() < header_bytes {
                return Err(ChdError::Malformed("CD hunk header is truncated".into()));
            }
            let mut complen_base = 0usize;
            for &b in &src[ecc_bytes..header_bytes] {
                complen_base = (complen_base << 8) | b as usize;
            }
            let base_end = header_bytes
                .checked_add(complen_base)
                .filter(|&e| e <= src.len())
                .ok_or_else(|| {
                    ChdError::Malformed("CD hunk base stream runs past the hunk".into())
                })?;
            let base = if codec == CODEC_CD_ZLIB {
                CODEC_ZLIB
            } else {
                CODEC_LZMA
            };
            match base {
                CODEC_ZLIB => inflate_raw(&src[header_bytes..base_end], sectors)?,
                _ => lzma_raw(&src[header_bytes..base_end], sectors, self.hunk_bytes)?,
            }
            // The subcode run is always deflated, whatever the base is.
            inflate_raw(&src[base_end..], subcodes)?;
        }

        // Re-interleave, restoring each flagged sector's sync header
        // and ECC parity (the compressor dropped them because they are
        // a pure function of the rest of the sector).
        for frame in 0..frames {
            let out = &mut dest[frame * CD_FRAME_SIZE..(frame + 1) * CD_FRAME_SIZE];
            out[..CD_MAX_SECTOR_DATA].copy_from_slice(
                &sectors[frame * CD_MAX_SECTOR_DATA..(frame + 1) * CD_MAX_SECTOR_DATA],
            );
            out[CD_MAX_SECTOR_DATA..].copy_from_slice(
                &subcodes[frame * CD_MAX_SUBCODE_DATA..(frame + 1) * CD_MAX_SUBCODE_DATA],
            );
            if ecc_bytes > 0 && src[frame / 8] & (1 << (frame % 8)) != 0 {
                let sector: &mut [u8; CD_MAX_SECTOR_DATA] = (&mut out[..CD_MAX_SECTOR_DATA])
                    .try_into()
                    .expect("fixed sector length");
                sector[..12].copy_from_slice(&CD_SYNC_HEADER);
                ecc_generate(sector);
            }
        }
        Ok(())
    }
}

/// Raw DEFLATE (no zlib wrapper — what CHD writes) producing exactly
/// `dest.len()` bytes.
fn inflate_raw(src: &[u8], dest: &mut [u8]) -> Result<(), ChdError> {
    use std::io::Read;
    let mut out = flate2::read::DeflateDecoder::new(src);
    let mut filled = 0;
    while filled < dest.len() {
        match out.read(&mut dest[filled..]) {
            Ok(0) => {
                return Err(ChdError::Malformed(format!(
                    "DEFLATE hunk ended after {filled} of {} bytes",
                    dest.len()
                )));
            }
            Ok(n) => filled += n,
            Err(e) => return Err(ChdError::Malformed(format!("DEFLATE hunk: {e}"))),
        }
    }
    Ok(())
}

/// The LZMA dictionary size chdman's encoder settled on for this hunk
/// size: 7-Zip's `LzmaEncProps_Normalize` at level 9, reduced to the
/// hunk. Only the *decoder's* window needs to be at least this, so
/// getting it exactly right costs nothing and drifting low would break
/// long matches.
fn lzma_dict_size(hunk_bytes: u32) -> u32 {
    let mut dict = 1u32 << 26; // level 9's default
    if dict > hunk_bytes {
        for i in 11..=30u32 {
            if hunk_bytes <= 2 << i {
                dict = 2 << i;
                break;
            }
            if hunk_bytes <= 3 << i {
                dict = 3 << i;
                break;
            }
        }
    }
    dict.max(hunk_bytes)
}

/// Raw LZMA1 — no `.lzma` header at all. chdman fixes lc=3 lp=0 pb=2
/// (7-Zip level 9) and hands the decoder the exact output size, so
/// there is no end-of-stream marker to find.
fn lzma_raw(src: &[u8], dest: &mut [u8], hunk_bytes: u32) -> Result<(), ChdError> {
    use lzma_rs::decompress::raw::{LzmaDecoder, LzmaParams, LzmaProperties};
    let props = LzmaProperties {
        lc: 3,
        lp: 0,
        pb: 2,
    };
    let params = LzmaParams::new(props, lzma_dict_size(hunk_bytes), Some(dest.len() as u64));
    let mut decoder = LzmaDecoder::new(params, None)
        .map_err(|e| ChdError::Malformed(format!("LZMA hunk: {e}")))?;
    let mut out: Vec<u8> = Vec::with_capacity(dest.len());
    let mut input = src;
    decoder
        .decompress(&mut input, &mut out)
        .map_err(|e| ChdError::Malformed(format!("LZMA hunk: {e}")))?;
    if out.len() != dest.len() {
        return Err(ChdError::Malformed(format!(
            "LZMA hunk produced {} bytes, not {}",
            out.len(),
            dest.len()
        )));
    }
    dest.copy_from_slice(&out);
    Ok(())
}

/// MAME's `huff` codec: a 256-symbol canonical Huffman tree exported
/// ahead of the byte stream it codes.
fn huffman_decode(src: &[u8], dest: &mut [u8]) -> Result<(), ChdError> {
    let mut bits = BitReader::new(src);
    let huff = Huffman::import_tree_huffman(256, 16, &mut bits)?;
    for slot in dest.iter_mut() {
        *slot = u8::try_from(huff.decode_one(&mut bits))
            .map_err(|_| ChdError::Malformed("huff hunk decoded a non-byte symbol".into()))?;
        if bits.overflowed() {
            return Err(ChdError::Malformed("huff hunk is truncated".into()));
        }
    }
    Ok(())
}

// ---- CD sector ECC regeneration (ECMA-130 14.2) ----
//
// Deliberately a local copy of the P/Q generator that `datboi-xf-ecm`
// also carries: that crate compiles to a shipped wasm component whose
// HASH is pinned by every ecm recipe ever minted, so widening its
// public surface to share ~40 lines would risk churning bytes that
// recipes name. The `ecc_agrees_with_the_ecm_component` test pins the
// two implementations together instead.

fn ecc_luts() -> &'static ([u8; 256], [u8; 256]) {
    use std::sync::OnceLock;
    static LUTS: OnceLock<([u8; 256], [u8; 256])> = OnceLock::new();
    LUTS.get_or_init(|| {
        let mut f = [0u8; 256];
        let mut b = [0u8; 256];
        for i in 0..256u32 {
            let j = (i << 1) ^ (if i & 0x80 != 0 { 0x11D } else { 0 });
            f[i as usize] = u8::try_from(j & 0xFF).expect("masked");
            b[(i ^ (j & 0xFF)) as usize] = u8::try_from(i).expect("i < 256");
        }
        (f, b)
    })
}

#[allow(clippy::many_single_char_names)]
fn ecc_pass(
    src: &[u8],
    zero_address: bool,
    major_count: usize,
    minor_count: usize,
    major_mult: usize,
    minor_inc: usize,
    dest: &mut [u8],
) {
    let (f_lut, b_lut) = ecc_luts();
    let size = major_count * minor_count;
    for major in 0..major_count {
        let mut index = (major >> 1) * major_mult + (major & 1);
        let mut ecc_a = 0u8;
        let mut ecc_b = 0u8;
        for _ in 0..minor_count {
            let temp = if zero_address && index < 4 {
                0
            } else {
                src[index]
            };
            index += minor_inc;
            if index >= size {
                index -= size;
            }
            ecc_a = f_lut[(ecc_a ^ temp) as usize];
            ecc_b ^= temp;
        }
        ecc_a = b_lut[(f_lut[ecc_a as usize] ^ ecc_b) as usize];
        dest[major] = ecc_a;
        dest[major + major_count] = ecc_a ^ ecc_b;
    }
}

/// Regenerate a sector's P and Q parity in place. Mode 2 sectors zero
/// the four header bytes for the computation; mode 1 does not.
pub(crate) fn ecc_generate(sector: &mut [u8; CD_MAX_SECTOR_DATA]) {
    let zero_address = sector[15] == 2;
    let mut p = [0u8; 172];
    ecc_pass(&sector[12..12 + 2064], zero_address, 86, 24, 2, 86, &mut p);
    sector[2076..2248].copy_from_slice(&p);
    let mut q = [0u8; 104];
    ecc_pass(&sector[12..12 + 2236], zero_address, 52, 43, 86, 88, &mut q);
    sector[2248..2352].copy_from_slice(&q);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dict_size_follows_the_7zip_reduction() {
        // 8 CD frames: 19584 bytes falls in 3 << 13 = 24576.
        assert_eq!(lzma_dict_size(8 * 2448), 24576);
        // A 4 KiB hunk lands on the loop's first rung.
        assert_eq!(lzma_dict_size(4096), 4096);
        // Larger than level 9's default: clamped up to the hunk.
        assert_eq!(lzma_dict_size(1 << 27), 1 << 27);
    }

    #[test]
    fn unsupported_codecs_are_named_not_guessed() {
        let mut dec = Decoder::new(4096);
        let mut out = [0u8; 16];
        let err = dec
            .decode(super::super::header::CODEC_AVHUFF, &[], &mut out)
            .unwrap_err();
        assert!(
            matches!(err, ChdError::Unsupported(ref m) if m.contains("avhu")),
            "{err:?}"
        );
        assert!(!supported(super::super::header::CODEC_AVHUFF));
    }

    #[test]
    fn ecc_agrees_with_the_ecm_component() {
        // A mode 1 and a mode 2 form 1 sector, built by the component's
        // own rebuilder, must survive a sync+parity wipe and our
        // regeneration byte-for-byte.
        for (kind, len) in [(1u8, 3 + 2048usize), (2, 3 + 8 + 2048)] {
            let stripped: Vec<u8> = (0..len)
                .map(|i| u8::try_from(i % 251).expect("< 256"))
                .collect();
            let original = datboi_xf_ecm::rebuild_sector(kind, &stripped);
            let mut scratch = original;
            scratch[..12].fill(0);
            scratch[2076..].fill(0);
            scratch[..12].copy_from_slice(&CD_SYNC_HEADER);
            ecc_generate(&mut scratch);
            assert_eq!(scratch, original, "kind {kind}");
        }
    }
}
