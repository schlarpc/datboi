//! NARC (Nitro Archive) reader — the .nds INTERIOR decomposition lane
//! (decomposition-arc step 3). A NARC is a self-contained Nitro
//! filesystem: a BTAF (the FAT), a BTNF (the FNT), and a GMIF (the file
//! image). Its members are byte ranges of the GMIF data — pure
//! concatenation with alignment padding, exactly the .nds container
//! shape one level down — so decomposition is the SAME coverage-map
//! arithmetic and every recipe is a builtin (no wasm, per the
//! open-questions NARC clause). We reuse the nds coverage machinery
//! (`classify_gap`, `Piece`, `Region`): the structural prefix (header +
//! BTAF + BTNF + GMIF header) and inter-member padding fall out as gap
//! classification, no special-casing.
//!
//! Why: two regional ROM variants that differ only INSIDE a NARC (a
//! localized text/graphics archive) share nothing at the NitroFS-file
//! boundary — the whole NARC differs — but share almost everything at
//! the NARC-MEMBER boundary. Decomposing the NARC recovers that dedup
//! exactly, before CDC has to chew the media-stream remainder (the
//! sequencing note in open-questions / the D59 rank-7 amendment).
//!
//! Bit-faithfulness rests on replay verification (D4), not this parser;
//! its burden is REFUSAL (D81): any structural anomaly is a settled
//! Negative, never an environmental error.

use std::io::{Read, Seek, SeekFrom};

use crate::nds::{NdsError, Piece, Refusal, Region, classify_gap, read_at, u16_at, u32_at};

const NARC_MAGIC: &[u8; 4] = b"NARC";
/// Little-endian byte-order mark every Nitro container carries.
const BOM_LE: u16 = 0xFFFE;
const HEADER_LEN: u64 = 0x10;
/// Same GBATEK FAT cap the ROM's NitroFS obeys.
const MAX_MEMBERS: u64 = 61_440;
/// A block "size" past this is a lie, not a real archive: NARC tables
/// (FAT + FNT) are small even when the file data is large.
const MAX_BLOCK_LEN: u64 = 64 << 20;

/// A NARC's exact coverage map: members as pieces, structural blocks and
/// padding as regions, concatenating to `[0, narc_len)`.
#[derive(Debug)]
pub struct Layout {
    pub narc_len: u64,
    /// Physical (coverage) order.
    pub pieces: Vec<Piece>,
    pub regions: Vec<Region>,
    /// Non-empty members (the payload that dedupes).
    pub file_count: usize,
    /// Zero-length members (identity = the empty blob).
    pub empty_files: usize,
    /// Bytes that had to become residual pieces/literals (structural
    /// prefix + non-uniform padding); coverage is still exact.
    pub residual_bytes: u64,
}

/// Cheap sniff: the "NARC" magic plus the 0xFFFE byte-order mark — two
/// checks over caller bytes, enough to admit a blob to the parser
/// without a false-positive flood.
#[must_use]
pub fn looks_like_narc(head: &[u8]) -> bool {
    head.len() >= HEADER_LEN as usize && &head[0..4] == NARC_MAGIC && u16_at(head, 4) == BOM_LE
}

/// Parse a NARC into its coverage map. `narc` is any seekable byte
/// source (the stored member blob — read through the executor for an
/// absent one, D92).
///
/// # Errors
/// [`NdsError::Refused`] is a settled conclusion about the bytes;
/// [`NdsError::Io`] is environmental.
pub fn parse_layout<R: Read + Seek>(narc: &mut R) -> Result<Layout, NdsError> {
    let narc_len = narc.seek(SeekFrom::End(0))?;
    if narc_len < HEADER_LEN {
        return Err(Refusal::Truncated("shorter than the NARC header").into());
    }
    let header = read_at(narc, 0, HEADER_LEN as usize)?;
    if &header[0..4] != NARC_MAGIC || u16_at(&header, 4) != BOM_LE {
        return Err(Refusal::NotNarc.into());
    }
    let header_len = u64::from(u16_at(&header, 0x0C));
    let num_blocks = u16_at(&header, 0x0E);
    if header_len < HEADER_LEN || header_len > narc_len {
        return Err(Refusal::Narc("header size out of bounds".into()).into());
    }
    if num_blocks < 3 {
        return Err(Refusal::Narc("fewer than the three NARC blocks".into()).into());
    }

    // Walk blocks by their self-declared sizes, capturing the FAT (BTAF)
    // and the file-image origin (GMIF data begins 8 bytes into it).
    let mut off = header_len;
    let mut fat_pairs: Vec<(u64, u64)> = Vec::new();
    let mut have_fat = false;
    let mut gmif_data: Option<u64> = None;
    for _ in 0..num_blocks {
        if off + 8 > narc_len {
            return Err(Refusal::Truncated("block header past EOF").into());
        }
        let bh = read_at(narc, off, 8)?;
        let size = u32_at(&bh, 4);
        if !(8..=MAX_BLOCK_LEN).contains(&size) || off + size > narc_len {
            return Err(Refusal::Narc("block size out of bounds".into()).into());
        }
        match &bh[0..4] {
            b"BTAF" => {
                if off + 12 > narc_len {
                    return Err(Refusal::Truncated("BTAF header").into());
                }
                let count = u64::from(u16_at(&read_at(narc, off + 8, 4)?, 0));
                if count > MAX_MEMBERS {
                    return Err(Refusal::Narc("member count past the FAT cap".into()).into());
                }
                let table_len = count * 8;
                if 12 + table_len > size {
                    return Err(Refusal::Narc("member table exceeds the BTAF block".into()).into());
                }
                let table = read_at(narc, off + 12, table_len as usize)?;
                for i in 0..count as usize {
                    fat_pairs.push((u32_at(&table, i * 8), u32_at(&table, i * 8 + 4)));
                }
                have_fat = true;
            }
            b"GMIF" => gmif_data = Some(off + 8),
            b"BTNF" => {} // FNT: names only; member slicing needs the FAT alone.
            other => {
                return Err(Refusal::Narc(format!(
                    "unknown block {}",
                    String::from_utf8_lossy(other)
                ))
                .into());
            }
        }
        off += size;
    }
    if !have_fat {
        return Err(Refusal::Narc("no BTAF (FAT) block".into()).into());
    }
    let Some(gmif_data) = gmif_data else {
        return Err(Refusal::Narc("no GMIF (file image) block".into()).into());
    };
    if fat_pairs.is_empty() {
        return Err(Refusal::Narc("empty FAT".into()).into());
    }

    // Members: absolute ranges into the NARC; zero-length = empty blob.
    let mut declared: Vec<Piece> = Vec::new();
    let mut empty_files = 0usize;
    for (i, (start, end)) in fat_pairs.iter().enumerate() {
        if end < start {
            return Err(Refusal::Narc(format!("member {i}: end before start")).into());
        }
        let abs_start = gmif_data
            .checked_add(*start)
            .ok_or_else(|| Refusal::Narc("member offset overflow".into()))?;
        let abs_end = gmif_data
            .checked_add(*end)
            .ok_or_else(|| Refusal::Narc("member offset overflow".into()))?;
        if abs_end > narc_len {
            return Err(Refusal::Narc(format!("member {i} runs past EOF")).into());
        }
        let len = abs_end - abs_start;
        if len == 0 {
            empty_files += 1;
            continue;
        }
        declared.push(Piece {
            name: format!("narc/{i:05}.bin"),
            start: abs_start,
            len,
        });
    }
    let file_count = declared.len();
    declared.sort_by_key(|p| p.start);
    for pair in declared.windows(2) {
        if pair[1].start < pair[0].start + pair[0].len {
            return Err(Refusal::Overlap(pair[0].name.clone(), pair[1].name.clone()).into());
        }
    }

    // Coverage walk: the same as the nds container one level up. Gap
    // classification absorbs the structural prefix and inter-member
    // padding — uniform runs become Fill (zero storage), the rest inline
    // or residual-piece.
    let mut pieces: Vec<Piece> = Vec::new();
    let mut regions: Vec<Region> = Vec::new();
    let mut residual_bytes = 0u64;
    let mut cursor = 0u64;
    for piece in declared {
        classify_gap(
            narc,
            cursor,
            piece.start - cursor,
            &mut regions,
            &mut pieces,
            &mut residual_bytes,
        )?;
        cursor = piece.start + piece.len;
        regions.push(Region::Piece(pieces.len()));
        pieces.push(piece);
    }
    classify_gap(
        narc,
        cursor,
        narc_len - cursor,
        &mut regions,
        &mut pieces,
        &mut residual_bytes,
    )?;

    // A NARC is mostly member data; if the "structure" dominates, this
    // was not a NARC in any useful sense — stay a literal.
    if residual_bytes > narc_len / 2 {
        return Err(Refusal::ExcessResidue {
            residual: residual_bytes,
            total: narc_len,
        }
        .into());
    }

    Ok(Layout {
        narc_len,
        pieces,
        regions,
        file_count,
        empty_files,
        residual_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A minimal NARC with the given FAT pairs and `data_len` bytes of
    /// GMIF file image (filled with a constant so gaps classify as Fill).
    fn narc(fat: &[(u32, u32)], data_len: usize) -> Vec<u8> {
        let mut btaf = Vec::new();
        btaf.extend_from_slice(b"BTAF");
        btaf.extend_from_slice(&((12 + fat.len() * 8) as u32).to_le_bytes());
        btaf.extend_from_slice(&(fat.len() as u16).to_le_bytes());
        btaf.extend_from_slice(&0u16.to_le_bytes());
        for (s, e) in fat {
            btaf.extend_from_slice(&s.to_le_bytes());
            btaf.extend_from_slice(&e.to_le_bytes());
        }
        let mut btnf = Vec::new();
        btnf.extend_from_slice(b"BTNF");
        btnf.extend_from_slice(&16u32.to_le_bytes());
        btnf.extend_from_slice(&4u32.to_le_bytes());
        btnf.extend_from_slice(&0u16.to_le_bytes());
        btnf.extend_from_slice(&1u16.to_le_bytes());
        let mut gmif = Vec::new();
        gmif.extend_from_slice(b"GMIF");
        gmif.extend_from_slice(&((8 + data_len) as u32).to_le_bytes());
        gmif.extend(std::iter::repeat_n(0xABu8, data_len));
        let total = (0x10 + btaf.len() + btnf.len() + gmif.len()) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"NARC");
        out.extend_from_slice(&0xFFFEu16.to_le_bytes());
        out.extend_from_slice(&0x0100u16.to_le_bytes());
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&0x10u16.to_le_bytes());
        out.extend_from_slice(&3u16.to_le_bytes());
        out.extend_from_slice(&btaf);
        out.extend_from_slice(&btnf);
        out.extend_from_slice(&gmif);
        out
    }

    #[test]
    fn sniff_needs_magic_and_bom() {
        assert!(looks_like_narc(&narc(&[(0, 4)], 4)));
        assert!(!looks_like_narc(b"NOPE and a bunch more bytes here"));
        assert!(!looks_like_narc(b"NARC")); // shorter than the header
    }

    #[test]
    fn refuses_overlapping_members() {
        // [0,8) and [4,8) share bytes — the shape D81 refuses outright.
        let err = parse_layout(&mut Cursor::new(narc(&[(0, 8), (4, 8)], 8))).unwrap_err();
        assert!(matches!(err, NdsError::Refused(Refusal::Overlap(..))));
    }

    #[test]
    fn refuses_member_past_eof() {
        let err = parse_layout(&mut Cursor::new(narc(&[(0, 999)], 8))).unwrap_err();
        assert!(matches!(err, NdsError::Refused(Refusal::Narc(_))));
    }

    #[test]
    fn refuses_non_narc() {
        let err = parse_layout(&mut Cursor::new(b"not even close to a narc header!".to_vec()))
            .unwrap_err();
        assert!(matches!(err, NdsError::Refused(Refusal::NotNarc)));
    }
}
