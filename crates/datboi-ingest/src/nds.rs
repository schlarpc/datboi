//! Nintendo DS ROM layout reader (D83): parse the NTR header + NitroFS
//! tables into an EXACT coverage map over the ROM bytes, so the
//! `nds-split/1` analyzer can decompose a .nds into `assemble@1` pieces
//! (per-file derive claims, a bit-faithful rebuild recipe) and compute
//! the safe trim length.
//!
//! An NTR-era ROM is a pure concatenation — header, ARM9/ARM7 binaries,
//! FNT/FAT/overlay tables/banner, then NitroFS files at absolute FAT
//! offsets, pad bytes in the gaps — nothing compressed or encrypted at
//! the container level, so every relationship between the ROM and its
//! parts is byte-range arithmetic and every recipe is a builtin. Files
//! are NOT guaranteed to be stored in FAT-ID order (retail ROMs violate
//! it), which is why the coverage map records physical order.
//!
//! Bit-faithfulness is not this parser's burden: the minted rebuild
//! recipe is verified by replay (D4), so a wrong map fails verification
//! and the ROM stays a harmless literal. The parser's burden is REFUSAL
//! (D81): any structural anomaly — overlapping ranges, unparseable
//! tables, excess bytes outside declared structure — is a deterministic
//! conclusion about the bytes ([`NdsError::Refused`]), never an
//! environmental error.
//!
//! Field offsets follow GBATEK (DS Cartridge Header / NitroROM); trim
//! rules follow GodMode9/ndstrim: DSi-inclusive ROMs (unitcode bit 1)
//! trim at the NTR+TWL size word `[210h]`, plain NTR at `[80h]` plus the
//! 88h-byte DS Download Play RSA block when its "ac" magic sits at that
//! offset — the block naive trimmers strip, breaking Download Play.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};

use datboi_core::assemble::LITERAL_CAP;

/// NTR header bytes the rebuild recipe inlines as a literal segment:
/// per-ROM unique (it embeds checksums and absolute offsets), so a blob
/// would never dedupe.
pub const HEAD_LEN: usize = 0x200;

/// GBATEK cap: FAT holds up to 61440 files.
const MAX_FAT_ENTRIES: u64 = 61_440;
/// GBATEK cap: FNT main table holds up to 4096 directories.
const MAX_DIRS: u16 = 4096;
/// Structural-table read ceilings — a "size" field past these is a lie,
/// not a big ROM (the largest retail FNT/FAT are a few hundred KiB).
const MAX_FNT_LEN: u64 = 8 << 20;
const MAX_OVERLAY_TABLE_LEN: u64 = 1 << 20;
/// A trailing pad run in a mixed gap is split into its own Fill segment
/// when at least this long (below it, inlining the whole gap is cheaper
/// than an extra segment).
const MIN_PAD_SPLIT: u64 = 512;
/// The Download Play / cloneboot RSA signature block after the declared
/// NTR ROM size: 2-byte "ac" magic, 88h bytes total.
const RSA_SIG_MAGIC: u16 = 0x6361;
const RSA_SIG_LEN: u64 = 0x88;

const SCAN_CHUNK: usize = 64 * 1024;

/// Structural refusals — deterministic conclusions about the bytes
/// (D81: recorded as Negative, settled, never retried).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("not an nds rom (header checksums do not verify)")]
    NotNds,
    #[error("not a narc (magic or byte-order mark absent)")]
    NotNarc,
    #[error("narc: {0}")]
    Narc(String),
    #[error("truncated: {0}")]
    Truncated(&'static str),
    #[error("header field out of bounds: {0}")]
    Bounds(&'static str),
    #[error("fat: {0}")]
    Fat(String),
    #[error("fnt: {0}")]
    Fnt(String),
    #[error("overlay table: {0}")]
    Overlay(String),
    #[error("declared ranges overlap: {0} and {1}")]
    Overlap(String, String),
    #[error("{residual} of {total} bytes fall outside the declared structure — not NitroFS-shaped")]
    ExcessResidue { residual: u64, total: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum NdsError {
    /// Environmental (retryable): the bytes could not be read.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A conclusion about the bytes (settled).
    #[error(transparent)]
    Refused(#[from] Refusal),
}

/// One blob-worthy byte range of the ROM: a binary, a table, a NitroFS
/// file, the trailing RSA block, or a residual gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// Label for the derive recipe's output (FNT path for files,
    /// ndstool-style names for system pieces, `gap@0x…` for residue).
    pub name: String,
    pub start: u64,
    pub len: u64,
}

/// One span of the coverage map. Regions concatenate to exactly
/// `[0, rom_len)` in order — the rebuild recipe is this list, verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region {
    /// Index into [`Layout::pieces`].
    Piece(usize),
    /// A uniform pad run — zero storage (dedupe ladder rank 5).
    Fill { byte: u8, len: u64 },
    /// Short raw bytes inlined into the recipe (≤ assemble's literal
    /// cap): the header, and small non-uniform gaps like ARM9 post-data
    /// or the DSi extended header.
    Literal { start: u64, len: u64 },
}

#[derive(Debug)]
pub struct Layout {
    pub rom_len: u64,
    /// In physical (coverage) order.
    pub pieces: Vec<Piece>,
    pub regions: Vec<Region>,
    /// Byte length of the safe trimmed view — `Some` only when the trim
    /// rules validate AND the discarded tail is verifiably pure pad
    /// (`None` for already-trimmed ROMs: nothing to mint).
    pub trim_len: Option<u64>,
    /// Non-empty FAT entries (the NitroFS payload).
    pub file_count: usize,
    /// Zero-length FAT entries (identity = the empty blob).
    pub empty_files: usize,
    /// Gap bytes that had to become pieces or literals — coverage is
    /// still exact, these just don't dedupe.
    pub residual_bytes: u64,
}

/// CRC-16 as the NDS BIOS computes it (poly 8005h reflected, init
/// FFFFh — the MODBUS variant): the header stores it over `[0, 15Eh)`
/// at 15Eh and over the logo at 15Ch.
#[must_use]
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bytes {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// True if `head` (≥ 160h bytes) carries a valid NDS header: both the
/// logo CRC and the header CRC verify against their stored values. Two
/// independent 16-bit checks over caller-controlled bytes — strong
/// enough to sniff every blob in a sweep without false positives.
#[must_use]
pub fn looks_like_nds(head: &[u8]) -> bool {
    if head.len() < 0x160 {
        return false;
    }
    crc16(&head[0xC0..0x15C]) == u16_at(head, 0x15C) && crc16(&head[..0x15E]) == u16_at(head, 0x15E)
}

/// Parse the full layout. `rom` is any seekable byte source (the stored
/// container blob); the whole ROM is read once (gap classification) plus
/// the structural tables.
///
/// # Errors
/// [`NdsError::Refused`] is a settled conclusion about the bytes;
/// [`NdsError::Io`] is environmental.
pub fn parse_layout<R: Read + Seek>(rom: &mut R) -> Result<Layout, NdsError> {
    let rom_len = rom.seek(SeekFrom::End(0))?;
    if rom_len < HEAD_LEN as u64 {
        return Err(Refusal::Truncated("shorter than the 200h header").into());
    }
    let header = read_at(rom, 0, HEAD_LEN)?;
    if !looks_like_nds(&header) {
        return Err(Refusal::NotNds.into());
    }
    let is_dsi = header[0x12] & 0x02 != 0;

    // Declared sections, straight from the header. Zero offset or zero
    // size = section absent (homebrew commonly has no FNT/banner).
    let mut declared: Vec<Piece> = Vec::new();
    let mut push = |name: &str, start: u64, len: u64| -> Result<(), Refusal> {
        if start == 0 || len == 0 {
            return Ok(());
        }
        let end = start.checked_add(len).ok_or(Refusal::Bounds("overflow"))?;
        if start < HEAD_LEN as u64 || end > rom_len {
            return Err(Refusal::Bounds("section outside the rom"));
        }
        declared.push(Piece {
            name: name.to_owned(),
            start,
            len,
        });
        Ok(())
    };
    push("arm9.bin", u32_at(&header, 0x20), u32_at(&header, 0x2C))?;
    push("arm7.bin", u32_at(&header, 0x30), u32_at(&header, 0x3C))?;
    let (fnt_off, fnt_len) = (u32_at(&header, 0x40), u32_at(&header, 0x44));
    push("fnt.bin", fnt_off, fnt_len)?;
    let (fat_off, fat_len) = (u32_at(&header, 0x48), u32_at(&header, 0x4C));
    push("fat.bin", fat_off, fat_len)?;
    let (y9_off, y9_len) = (u32_at(&header, 0x50), u32_at(&header, 0x54));
    push("y9.bin", y9_off, y9_len)?;
    let (y7_off, y7_len) = (u32_at(&header, 0x58), u32_at(&header, 0x5C));
    push("y7.bin", y7_off, y7_len)?;
    let banner_off = u32_at(&header, 0x68);
    if banner_off != 0 && banner_off + 2 <= rom_len {
        // Banner length is keyed by its version word (GBATEK); an
        // unknown version stays undeclared and lands in gap residue.
        let version = u16_at(&read_at(rom, banner_off, 2)?, 0);
        let banner_len: u64 = match version {
            0x0001 => 0x840,
            0x0002 => 0x940,
            0x0003 => 0xA40,
            0x0103 => 0x23C0,
            _ => 0,
        };
        push("banner.bin", banner_off, banner_len)?;
    }
    if is_dsi {
        // TWL binaries + the stored digest hashtables. The digest
        // *region* fields (1E0h/1E8h) describe covered areas, not
        // stored tables — declaring them would overlap everything.
        push("arm9i.bin", u32_at(&header, 0x1C0), u32_at(&header, 0x1CC))?;
        push("arm7i.bin", u32_at(&header, 0x1D0), u32_at(&header, 0x1DC))?;
        push(
            "hashtable_sector.bin",
            u32_at(&header, 0x1F0),
            u32_at(&header, 0x1F4),
        )?;
        push(
            "hashtable_block.bin",
            u32_at(&header, 0x1F8),
            u32_at(&header, 0x1FC),
        )?;
    }

    // FAT: 8 bytes per file, (start, end-exclusive) absolute offsets.
    let mut file_count = 0usize;
    let mut empty_files = 0usize;
    let mut fat_entries: Vec<Option<(u64, u64)>> = Vec::new();
    if fat_len > 0 {
        if fat_len % 8 != 0 {
            return Err(Refusal::Fat("size not a multiple of 8".into()).into());
        }
        if fat_len / 8 > MAX_FAT_ENTRIES {
            return Err(Refusal::Fat(format!(
                "{} entries exceeds the {MAX_FAT_ENTRIES} cap",
                fat_len / 8
            ))
            .into());
        }
        let fat = read_at(rom, fat_off, fat_len as usize)?;
        for (id, entry) in fat.chunks_exact(8).enumerate() {
            let (start, end) = (u32_at(entry, 0), u32_at(entry, 4));
            if start == 0 && end == 0 {
                fat_entries.push(None); // unused slot
                continue;
            }
            if end < start || end > rom_len {
                return Err(Refusal::Fat(format!("entry {id} out of bounds")).into());
            }
            if start < HEAD_LEN as u64 {
                return Err(Refusal::Fat(format!("entry {id} inside the header")).into());
            }
            if end == start {
                empty_files += 1;
                fat_entries.push(None);
                continue;
            }
            file_count += 1;
            fat_entries.push(Some((start, end)));
        }
    }

    // Names: FNT paths first, overlay-table labels for the FAT ids the
    // FNT hides, `fat/NNNN.bin` for anything left.
    let mut names: HashMap<u16, String> = HashMap::new();
    if fnt_len > 0 {
        if fnt_len > MAX_FNT_LEN {
            return Err(Refusal::Fnt("implausibly large".into()).into());
        }
        names = parse_fnt(&read_at(rom, fnt_off, fnt_len as usize)?)?;
    }
    for (which, off, len) in [(9u8, y9_off, y9_len), (7u8, y7_off, y7_len)] {
        if len == 0 {
            continue;
        }
        if len > MAX_OVERLAY_TABLE_LEN {
            return Err(Refusal::Overlay("implausibly large".into()).into());
        }
        overlay_names(
            &read_at(rom, off, len as usize)?,
            which,
            fat_entries.len() as u64,
            &mut names,
        )?;
    }
    for (id, entry) in fat_entries.iter().enumerate() {
        let Some((start, end)) = entry else { continue };
        let id16 = u16::try_from(id).expect("bounded by the FAT cap");
        let name = names
            .remove(&id16)
            .unwrap_or_else(|| format!("fat/{id:04}.bin"));
        declared.push(Piece {
            name,
            start: *start,
            len: end - start,
        });
    }

    declared.sort_by_key(|p| p.start);
    for pair in declared.windows(2) {
        if pair[1].start < pair[0].start + pair[0].len {
            return Err(Refusal::Overlap(pair[0].name.clone(), pair[1].name.clone()).into());
        }
    }

    // The trailing Download Play RSA block (NTR only): carve it as its
    // own piece when the "ac" magic sits exactly at the declared ROM
    // size — but never at the cost of the whole decomposition (a fake
    // size word pointing inside a file skips the carve, not the split).
    let ntr_size = u32_at(&header, 0x80);
    let mut has_rsa_sig = false;
    if !is_dsi
        && ntr_size + 2 <= rom_len
        && ntr_size
            .checked_add(RSA_SIG_LEN)
            .is_some_and(|e| e <= rom_len)
        && u16_at(&read_at(rom, ntr_size, 2)?, 0) == RSA_SIG_MAGIC
    {
        let sig = Piece {
            name: "rsa_sig.bin".into(),
            start: ntr_size,
            len: RSA_SIG_LEN,
        };
        let ix = declared.partition_point(|p| p.start < sig.start);
        let clear = (ix == 0 || declared[ix - 1].start + declared[ix - 1].len <= sig.start)
            && declared
                .get(ix)
                .is_none_or(|next| sig.start + sig.len <= next.start);
        if clear {
            has_rsa_sig = true;
            declared.insert(ix, sig);
        }
    }
    let data_end = declared
        .iter()
        .map(|p| p.start + p.len)
        .max()
        .unwrap_or(HEAD_LEN as u64);

    // Coverage walk: header literal, then declared pieces with every
    // gap classified by reading it — uniform runs become Fill (zero
    // storage), short mixed spans inline, the rest are residual pieces.
    let mut pieces: Vec<Piece> = Vec::new();
    let mut regions: Vec<Region> = vec![Region::Literal {
        start: 0,
        len: HEAD_LEN as u64,
    }];
    let mut residual_bytes = 0u64;
    let mut cursor = HEAD_LEN as u64;
    for piece in declared {
        classify_gap(
            rom,
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
        rom,
        cursor,
        rom_len - cursor,
        &mut regions,
        &mut pieces,
        &mut residual_bytes,
    )?;

    if residual_bytes > rom_len / 4 {
        return Err(Refusal::ExcessResidue {
            residual: residual_bytes,
            total: rom_len,
        }
        .into());
    }

    // Trim (D83): DSi-inclusive ROMs trim at [210h] (NTR+TWL total —
    // [80h] would cut the TWL region), plain NTR at [80h] plus the RSA
    // block when present. Offered only when the size word clears every
    // declared range AND the discarded tail is verifiably pure pad —
    // fake size words (the "Egg Monster Hero test") and data appended
    // past the header size (translation patches) both fail these gates.
    let candidate = if is_dsi {
        if rom_len >= 0x214 {
            u32_at(&read_at(rom, 0x210, 4)?, 0)
        } else {
            0
        }
    } else {
        ntr_size + if has_rsa_sig { RSA_SIG_LEN } else { 0 }
    };
    let trim_len = (candidate >= data_end
        && candidate >= HEAD_LEN as u64
        && candidate < rom_len
        && tail_is_pad(&regions, &pieces, candidate))
    .then_some(candidate);

    Ok(Layout {
        rom_len,
        pieces,
        regions,
        trim_len,
        file_count,
        empty_files,
        residual_bytes,
    })
}

/// Walk the FNT: main table (8 bytes per directory, ids F000h+), then
/// each directory's sub-table (length-prefixed names; files get
/// sequential ids from the directory's first-file-id). Returns
/// file-id → path. Any inconsistency refuses the whole table.
fn parse_fnt(fnt: &[u8]) -> Result<HashMap<u16, String>, Refusal> {
    let refuse = |what: &str| Refusal::Fnt(what.to_owned());
    if fnt.len() < 8 {
        return Err(refuse("main table shorter than one entry"));
    }
    let total = u16_at(fnt, 6);
    if total == 0 || total > MAX_DIRS {
        return Err(refuse("directory count outside 1..=4096"));
    }
    if usize::from(total) * 8 > fnt.len() {
        return Err(refuse("main table extends past the FNT"));
    }
    let mut names = HashMap::new();
    let mut visited = vec![false; usize::from(total)];
    let mut stack: Vec<(u16, String)> = vec![(0, String::new())];
    while let Some((dir_ix, path)) = stack.pop() {
        let slot = usize::from(dir_ix);
        if visited[slot] {
            return Err(refuse("directory graph is not a tree"));
        }
        visited[slot] = true;
        let base = slot * 8;
        let mut pos = u32_at_usize(fnt, base);
        let mut file_id = u16_at(fnt, base + 4);
        loop {
            let &kind = fnt.get(pos).ok_or_else(|| refuse("sub-table truncated"))?;
            pos += 1;
            if kind == 0 {
                break;
            }
            let name_len = usize::from(kind & 0x7F);
            if name_len == 0 {
                return Err(refuse("zero-length name"));
            }
            let name_bytes = fnt
                .get(pos..pos + name_len)
                .ok_or_else(|| refuse("name truncated"))?;
            pos += name_len;
            // Lossy on purpose: claims need a stable label, not a
            // faithful filesystem round-trip (the zip precedent).
            let name = String::from_utf8_lossy(name_bytes);
            let joined = if path.is_empty() {
                name.into_owned()
            } else {
                format!("{path}/{name}")
            };
            if kind & 0x80 != 0 {
                let id_bytes = fnt
                    .get(pos..pos + 2)
                    .ok_or_else(|| refuse("directory id truncated"))?;
                pos += 2;
                let id = u16::from_le_bytes([id_bytes[0], id_bytes[1]]);
                if !(0xF000..0xF000 + total).contains(&id) {
                    return Err(refuse("directory id out of range"));
                }
                stack.push((id - 0xF000, joined));
            } else {
                names.insert(file_id, joined);
                file_id = file_id
                    .checked_add(1)
                    .ok_or_else(|| refuse("file id overflow"))?;
                if names.len() as u64 > MAX_FAT_ENTRIES {
                    return Err(refuse("more names than the FAT cap"));
                }
            }
        }
    }
    Ok(names)
}

/// Overlay tables name the FAT ids the FNT hides: 20h-byte entries,
/// overlay id at +0, file id at +18h (validated against the FAT).
fn overlay_names(
    table: &[u8],
    which: u8,
    fat_count: u64,
    names: &mut HashMap<u16, String>,
) -> Result<(), Refusal> {
    if !table.len().is_multiple_of(0x20) {
        return Err(Refusal::Overlay("size not a multiple of 20h".into()));
    }
    for entry in table.chunks_exact(0x20) {
        let overlay_id = u32_at(entry, 0);
        let file_id = u32_at(entry, 0x18);
        if file_id >= fat_count {
            return Err(Refusal::Overlay(format!(
                "file id {file_id} not in the FAT"
            )));
        }
        names
            .entry(u16::try_from(file_id).expect("bounded by the FAT cap"))
            .or_insert_with(|| format!("overlay{which}/{overlay_id:04}.bin"));
    }
    Ok(())
}

/// Classify one gap `[start, start+len)` by reading it: uniform → Fill;
/// mixed with a long pad tail → head + Fill; short mixed → inline
/// literal; anything else → a residual piece (exact coverage, poor
/// dedupe — counted against the residue gate).
pub(crate) fn classify_gap<R: Read + Seek>(
    rom: &mut R,
    start: u64,
    len: u64,
    regions: &mut Vec<Region>,
    pieces: &mut Vec<Piece>,
    residual_bytes: &mut u64,
) -> Result<(), NdsError> {
    if len == 0 {
        return Ok(());
    }
    // One pass tracking the trailing uniform run; the gap is uniform
    // exactly when that run spans it.
    rom.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; SCAN_CHUNK.min(usize::try_from(len).unwrap_or(SCAN_CHUNK))];
    let mut remaining = len;
    let mut run_byte = 0u8;
    let mut run_len = 0u64;
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buf.len() as u64)).expect("bounded");
        let chunk = &mut buf[..want];
        rom.read_exact(chunk)?;
        remaining -= want as u64;
        let last = chunk[want - 1];
        let tail = want - chunk.iter().rposition(|&b| b != last).map_or(0, |p| p + 1);
        if tail == want && last == run_byte {
            run_len += tail as u64;
        } else {
            run_byte = last;
            run_len = tail as u64;
        }
    }

    let mut emit = |start: u64, len: u64, regions: &mut Vec<Region>, pieces: &mut Vec<Piece>| {
        *residual_bytes += len;
        if len <= LITERAL_CAP as u64 {
            regions.push(Region::Literal { start, len });
        } else {
            regions.push(Region::Piece(pieces.len()));
            pieces.push(Piece {
                name: format!("gap@0x{start:08x}"),
                start,
                len,
            });
        }
    };
    if run_len == len {
        regions.push(Region::Fill {
            byte: run_byte,
            len,
        });
    } else if run_len >= MIN_PAD_SPLIT {
        emit(start, len - run_len, regions, pieces);
        regions.push(Region::Fill {
            byte: run_byte,
            len: run_len,
        });
    } else {
        emit(start, len, regions, pieces);
    }
    Ok(())
}

/// True when everything at/after `candidate` is Fill — the discarded
/// tail of a trim must be verifiably pure pad.
fn tail_is_pad(regions: &[Region], pieces: &[Piece], candidate: u64) -> bool {
    let mut offset = 0u64;
    for region in regions {
        let len = match region {
            Region::Piece(ix) => pieces[*ix].len,
            Region::Fill { len, .. } | Region::Literal { len, .. } => *len,
        };
        if offset + len > candidate && !matches!(region, Region::Fill { .. }) {
            return false;
        }
        offset += len;
    }
    true
}

pub(crate) fn read_at<R: Read + Seek>(rom: &mut R, start: u64, len: usize) -> io::Result<Vec<u8>> {
    rom.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; len];
    rom.read_exact(&mut buf)?;
    Ok(buf)
}

pub(crate) fn u16_at(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}

pub(crate) fn u32_at(buf: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes([
        buf[at],
        buf[at + 1],
        buf[at + 2],
        buf[at + 3],
    ]))
}

fn u32_at_usize(buf: &[u8], at: usize) -> usize {
    usize::try_from(u32_at(buf, at)).expect("u32 fits usize")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC-16/MODBUS check value (the variant the header stores).
    #[test]
    fn crc16_is_the_modbus_variant() {
        assert_eq!(crc16(b"123456789"), 0x4B37);
    }

    #[test]
    fn fnt_walks_nested_directories() {
        // Root (2 dirs total): file "a", dir "sub" → F001; sub: "b.bin".
        let mut fnt = Vec::new();
        fnt.extend_from_slice(&16u32.to_le_bytes()); // root sub-table offset
        fnt.extend_from_slice(&0u16.to_le_bytes()); // first file id
        fnt.extend_from_slice(&2u16.to_le_bytes()); // total dirs
        fnt.extend_from_slice(&25u32.to_le_bytes()); // F001 sub-table offset
        fnt.extend_from_slice(&1u16.to_le_bytes()); // first file id
        fnt.extend_from_slice(&0xF000u16.to_le_bytes()); // parent
        fnt.extend_from_slice(&[0x01, b'a', 0x83]); // file "a", dir len 3
        fnt.extend_from_slice(b"sub");
        fnt.extend_from_slice(&0xF001u16.to_le_bytes());
        fnt.push(0);
        assert_eq!(fnt.len(), 25);
        fnt.extend_from_slice(&[0x05]);
        fnt.extend_from_slice(b"b.bin");
        fnt.push(0);

        let names = parse_fnt(&fnt).expect("valid fnt");
        assert_eq!(names[&0], "a");
        assert_eq!(names[&1], "sub/b.bin");
    }

    #[test]
    fn fnt_refuses_cycles() {
        // Root's sub-table names root itself as a child.
        let mut fnt = Vec::new();
        fnt.extend_from_slice(&8u32.to_le_bytes());
        fnt.extend_from_slice(&0u16.to_le_bytes());
        fnt.extend_from_slice(&1u16.to_le_bytes());
        fnt.extend_from_slice(&[0x81, b'x']);
        fnt.extend_from_slice(&0xF000u16.to_le_bytes());
        fnt.push(0);
        assert_eq!(
            parse_fnt(&fnt),
            Err(Refusal::Fnt("directory graph is not a tree".into()))
        );
    }
}
