//! ISO9660 volume reader (D113/D114): an ECMA-119 volume on a cooked
//! 2048-byte-sector image — the redump Xbox image's video partition,
//! or a whole PS2/PSP/PC disc image — walked into file and directory
//! extents so an analyzer can claim them as named pieces. Two callers:
//! `xdvdfs-split/1` walks the video partition (D113) and claims the
//! declared volume as a view; `iso9660-split/1` walks a top-level image
//! (D114) and [`parse_layout`] turns the walk into an exact coverage
//! map — files and directory tables as pieces, uniform files as fills,
//! every gap classified — for the shared decomposition mint path.
//!
//! Deliberately minimal: primary tree only — no Joliet, no Rock Ridge,
//! no UDF (each references the same extents the primary tree does;
//! walking two would double-claim; names come from the primary tree),
//! no path tables (derived from the tree; they classify as residue).
//! Multi-extent files (the ISO9660 shape of a > 4 GiB file) are one
//! piece per extent; interleaved files are refused — nothing on these
//! discs uses them.
//!
//! Structural anomalies are deterministic conclusions ([`Refusal`]),
//! never environmental errors (D81); a wrong map fails D4 replay, the
//! image stays literal.

use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom};

use crate::nds::{Piece, Region, classify_gap, read_at, u32_at};

pub const SECTOR: u64 = 2048;
/// The primary volume descriptor's sector.
pub const PVD_SECTOR: u64 = 16;
const PVD_TYPE: u8 = 1;
const ID: &[u8; 5] = b"CD001";
const FLAG_DIRECTORY: u8 = 0x02;
const FLAG_MULTI_EXTENT: u8 = 0x80;
const MAX_DEPTH: usize = 32;
const MAX_ENTRIES: usize = 1 << 16;
/// Volume descriptors run from sector 16 to a terminator; more than
/// this many is a lie.
const MAX_DESCRIPTORS: u64 = 64;
const TERMINATOR_TYPE: u8 = 0xFF;
/// Residue under this is fixed overhead (descriptors, path tables, UDF
/// metadata on a bridge disc), never evidence the tree is a stub — the
/// quarter-of-the-image gate applies above it.
const RESIDUE_FLOOR: u64 = 4 << 20;
/// A directory extent above this is a lie.
const MAX_DIR_EXTENT: u64 = 16 << 20;
/// Files at least this long are read once at layout time to see
/// whether they are uniform — a pad file (PS2 `DUMMY.DAT` and kin) is a
/// fill, not a piece worth packing. Shorter files are cheap either way.
const UNIFORM_FILE_MIN: u64 = 1 << 20;
/// Bytes per read while checking a file for uniformity.
const SCAN_CHUNK: usize = 1 << 20;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("no iso9660 primary volume descriptor at sector 16")]
    NotIso9660,
    #[error("iso9660: {0}")]
    Structure(String),
    #[error("truncated: {0}")]
    Truncated(&'static str),
    #[error("declared extents overlap: {0} and {1}")]
    Overlap(String, String),
    #[error("{residual} of {total} bytes fall outside the primary tree — not iso9660-shaped")]
    ExcessResidue { residual: u64, total: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum Iso9660Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Refused(#[from] Refusal),
}

/// One extent of the volume: a file or a directory's record table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `/VIDEO_TS/VTS_01_1.VOB` — version suffix (`;1`) stripped.
    pub path: String,
    /// Logical block address (2048-byte sectors from the volume start).
    pub lba: u64,
    pub len: u64,
    pub is_dir: bool,
}

#[derive(Debug)]
pub struct Volume {
    /// Volume space size from the PVD, in sectors.
    pub sectors: u64,
    /// Files and directory tables, in tree order (root table first). A
    /// multi-extent file is one entry per extent (`path`, `path#1`, …).
    pub entries: Vec<Entry>,
    pub file_count: usize,
    pub dir_count: usize,
    /// Zero-length files: no extent, nothing to claim.
    pub empty_files: usize,
    /// Files recorded as more than one extent.
    pub multi_extent_files: usize,
    /// Volume descriptor sectors from sector 16 through the set
    /// terminator (PVD, any supplementary/boot records, terminator).
    pub descriptor_sectors: u64,
}

/// True if `sector` (2048 bytes) is an ISO9660 primary volume descriptor.
#[must_use]
pub fn looks_like_pvd(sector: &[u8]) -> bool {
    sector.len() >= 7 && sector[0] == PVD_TYPE && &sector[1..6] == ID
}

/// Walk the volume at byte offset `base` of `img`. Every extent must
/// lie within `limit` bytes of `base` (the game partition's start on a
/// redump image): a volume claiming bytes past it is refused, not
/// trusted.
///
/// # Errors
/// [`Iso9660Error::Refused`] is a settled conclusion; `Io` is environmental.
pub fn parse_volume<R: Read + Seek>(
    img: &mut R,
    base: u64,
    limit: u64,
) -> Result<Volume, Iso9660Error> {
    if PVD_SECTOR * SECTOR + SECTOR > limit {
        return Err(Refusal::NotIso9660.into());
    }
    let pvd = read_at(img, base + PVD_SECTOR * SECTOR, 2048)?;
    if !looks_like_pvd(&pvd) {
        return Err(Refusal::NotIso9660.into());
    }
    let sectors = u32_at(&pvd, 80);
    let block = u16::from_le_bytes([pvd[128], pvd[129]]);
    if block != 2048 {
        return Err(Refusal::Structure(format!("logical block size {block}, not 2048")).into());
    }
    let root = &pvd[156..190];
    let (root_lba, root_len) = (u32_at(root, 2), u32_at(root, 10));
    if root[25] & FLAG_DIRECTORY == 0 {
        return Err(Refusal::Structure("root record is not a directory".into()).into());
    }
    // The descriptor set: sector 16 through the terminator.
    let mut descriptor_sectors = 1u64;
    loop {
        let at = (PVD_SECTOR + descriptor_sectors) * SECTOR;
        if at + SECTOR > limit || descriptor_sectors > MAX_DESCRIPTORS {
            return Err(Refusal::Structure("no volume descriptor set terminator".into()).into());
        }
        let vd = read_at(img, base + at, 7)?;
        // ECMA-167 volume recognition descriptors (a UDF bridge disc's
        // BEA01 / NSR0x / TEA01) may sit in the sequence before the
        // ISO9660 terminator on some masters.
        let ecma167 = matches!(
            &vd[1..6],
            b"BEA01" | b"NSR02" | b"NSR03" | b"TEA01" | b"BOOT2" | b"CDW02"
        );
        if &vd[1..6] != ID && !ecma167 {
            return Err(Refusal::Structure(format!(
                "descriptor {descriptor_sectors} lacks the CD001 identifier"
            ))
            .into());
        }
        descriptor_sectors += 1;
        if vd[0] == TERMINATOR_TYPE && &vd[1..6] == ID {
            break;
        }
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut file_count = 0usize;
    let mut dir_count = 0usize;
    let mut empty_files = 0usize;
    let mut multi_extent_files = 0usize;
    let mut stack: Vec<(u64, u64, String, usize)> = vec![(root_lba, root_len, String::new(), 0)];
    while let Some((lba, len, path, depth)) = stack.pop() {
        let refuse = |what: String| {
            Refusal::Structure(format!(
                "{}: {what}",
                if path.is_empty() { "/" } else { &path }
            ))
        };
        if depth > MAX_DEPTH {
            return Err(refuse("tree deeper than 32 levels".into()).into());
        }
        // A directory's declared length is its BYTE length: sector-
        // rounded on most masters, exact on others (an XGD2 video
        // partition declares its root as 194 bytes) — the same lesson
        // XDVDFS taught (D111 amendment). The table piece is the
        // declared bytes; the slack classifies like a file tail.
        if len == 0 || len > MAX_DIR_EXTENT {
            return Err(refuse(format!("directory extent of {len} bytes")).into());
        }
        let start = lba * SECTOR;
        if start + len > limit {
            return Err(refuse("directory extent past the partition".into()).into());
        }
        if !seen.insert(lba) {
            return Err(refuse("directory extent referenced twice".into()).into());
        }
        dir_count += 1;
        entries.push(Entry {
            path: if path.is_empty() {
                "/".to_owned()
            } else {
                path.clone()
            },
            lba,
            len,
            is_dir: true,
        });
        let table = read_at(img, base + start, usize::try_from(len).expect("capped"))?;
        let mut off = 0usize;
        // A multi-extent file is consecutive records sharing a name, all
        // but the last flagged; this carries (name, extent ordinal) from
        // one record to the next.
        let mut continued: Option<(String, u32)> = None;
        while off < table.len() {
            let rec_len = usize::from(table[off]);
            if rec_len == 0 {
                // Records never straddle a sector; the rest of this one
                // is padding.
                off = (off / 2048 + 1) * 2048;
                continue;
            }
            if rec_len < 33 || off + rec_len > table.len() {
                return Err(refuse(format!("record at {off} has length {rec_len}")).into());
            }
            let rec = &table[off..off + rec_len];
            off += rec_len;
            let name_len = usize::from(rec[32]);
            if 33 + name_len > rec_len {
                return Err(refuse(format!(
                    "record name of {name_len} bytes overruns the record"
                ))
                .into());
            }
            let name = &rec[33..33 + name_len];
            if name == [0] || name == [1] {
                continue; // `.` and `..`
            }
            let flags = rec[25];
            if rec[26] != 0 || rec[27] != 0 {
                return Err(refuse("interleaved file".into()).into());
            }
            let (ext_lba, ext_len) = (u32_at(rec, 2), u32_at(rec, 10));
            // Names are ASCII d-characters plus `;version`; keep the
            // path stable and drop the version (always `;1` in practice).
            let mut text = String::from_utf8_lossy(name).into_owned();
            if let Some(semi) = text.find(';') {
                text.truncate(semi);
            }
            let full = format!("{path}/{text}");
            if flags & FLAG_DIRECTORY != 0 {
                continued = None;
                stack.push((ext_lba, ext_len, full, depth + 1));
            } else {
                let ordinal = match continued.take() {
                    Some((prev, k)) if prev == full => k + 1,
                    _ => 0,
                };
                if flags & FLAG_MULTI_EXTENT != 0 {
                    continued = Some((full.clone(), ordinal));
                }
                if ordinal == 0 {
                    if ext_len == 0 && flags & FLAG_MULTI_EXTENT == 0 {
                        empty_files += 1;
                        continue;
                    }
                    file_count += 1;
                } else if ordinal == 1 {
                    multi_extent_files += 1;
                }
                if ext_len == 0 {
                    continue;
                }
                if ext_lba * SECTOR + ext_len > limit {
                    return Err(refuse(format!("{full} extends past the volume limit")).into());
                }
                entries.push(Entry {
                    path: if ordinal == 0 {
                        full
                    } else {
                        format!("{full}#{ordinal}")
                    },
                    lba: ext_lba,
                    len: ext_len,
                    is_dir: false,
                });
            }
            if entries.len() > MAX_ENTRIES {
                return Err(refuse("more entries than the tree cap".into()).into());
            }
        }
    }
    Ok(Volume {
        sectors,
        entries,
        file_count,
        dir_count,
        empty_files,
        multi_extent_files,
        descriptor_sectors,
    })
}

/// The exact coverage map of a whole cooked image (D114) — what
/// `iso9660-split/1` mints through the shared decomposition path.
#[derive(Debug)]
pub struct Layout {
    pub total_len: u64,
    /// Volume space size the PVD declares, in sectors (advisory: the
    /// image may run past it — a trailing pad — or, on a bad master,
    /// stop short; the map covers the IMAGE).
    pub declared_sectors: u64,
    /// In physical (coverage) order; regions reference these by index.
    pub pieces: Vec<Piece>,
    pub regions: Vec<Region>,
    pub file_count: usize,
    pub dir_count: usize,
    pub empty_files: usize,
    pub multi_extent_files: usize,
    /// Files whose bytes are one repeated value — fills, not pieces.
    pub uniform_files: usize,
    /// Uniform bytes (pads, slack, uniform files) — zero storage.
    pub fill_bytes: u64,
    /// Gap bytes that had to become pieces or literals: descriptors,
    /// path tables, anything the primary tree does not name.
    pub residual_bytes: u64,
    /// True when the entry count exceeded the piece cap and pieces are
    /// contiguous data runs instead of files.
    pub coalesced: bool,
}

/// One declared byte range of the image.
#[derive(Debug, Clone)]
struct Declared {
    name: String,
    start: u64,
    len: u64,
}

/// Parse the whole image as one ISO9660 volume and build its coverage
/// map: files and directory tables as pieces (or sector-aligned data
/// runs past `max_pieces`), uniform files as fills, every gap classified
/// (fill / inline literal / residue piece). Refuses when more than a
/// quarter of the image is residue — the primary tree does not describe
/// these bytes (a stub tree, or a container whose real structure is
/// something else, like a redump Xbox image whose game partition sits
/// behind the DVD-Video volume).
///
/// # Errors
/// [`Iso9660Error::Refused`] is a settled conclusion; `Io` is environmental.
pub fn parse_layout<R: Read + Seek>(
    img: &mut R,
    max_pieces: usize,
) -> Result<Layout, Iso9660Error> {
    let total_len = img.seek(SeekFrom::End(0))?;
    let vol = parse_volume(img, 0, total_len)?;

    let mut declared: Vec<Declared> = vol
        .entries
        .iter()
        .map(|e| Declared {
            name: if e.is_dir {
                format!("dir:{}", e.path)
            } else {
                e.path.clone()
            },
            start: e.lba * SECTOR,
            len: e.len,
        })
        .collect();
    // The descriptor set is data too (unique per disc: volume id,
    // timestamps, root pointer) — declared so the system area before
    // it classifies on its own.
    declared.push(Declared {
        name: "descriptors".into(),
        start: PVD_SECTOR * SECTOR,
        len: vol.descriptor_sectors * SECTOR,
    });
    // Order, dedupe exact aliases (two names over one extent are one
    // piece), refuse partial overlaps.
    declared.sort_by_key(|d| (d.start, d.len));
    declared.dedup_by(|b, a| a.start == b.start && a.len == b.len);
    for pair in declared.windows(2) {
        if pair[1].start < pair[0].start + pair[0].len {
            return Err(Refusal::Overlap(pair[0].name.clone(), pair[1].name.clone()).into());
        }
    }

    // A long uniform file is a fill: nothing to dedupe, nothing to pack.
    let mut uniform: Vec<Option<u8>> = Vec::with_capacity(declared.len());
    let mut uniform_files = 0usize;
    let mut buf = vec![0u8; SCAN_CHUNK];
    for d in &declared {
        let byte = if d.len >= UNIFORM_FILE_MIN && !d.name.starts_with("dir:") {
            uniform_byte(img, d.start, d.len, &mut buf)?
        } else {
            None
        };
        uniform_files += usize::from(byte.is_some());
        uniform.push(byte);
    }

    // Above the piece cap, pieces are contiguous data runs (sector
    // aligned) — recipe volume bounded, fills unchanged.
    let coalesced = declared.len() > max_pieces;
    let declared: Vec<(Declared, Option<u8>)> = if coalesced {
        let mut runs: Vec<(Declared, Option<u8>)> = Vec::new();
        for (d, byte) in declared.into_iter().zip(uniform) {
            let end = (d.start + d.len).div_ceil(SECTOR) * SECTOR;
            let start = d.start / SECTOR * SECTOR;
            match runs.last_mut() {
                Some((last, last_byte))
                    if byte.is_none() && last_byte.is_none() && start <= last.start + last.len =>
                {
                    last.len = end.max(last.start + last.len) - last.start;
                }
                _ => runs.push((
                    Declared {
                        name: if byte.is_some() {
                            d.name
                        } else {
                            format!("extent@0x{start:09x}")
                        },
                        start: if byte.is_some() { d.start } else { start },
                        len: if byte.is_some() { d.len } else { end - start },
                    },
                    byte,
                )),
            }
        }
        runs
    } else {
        declared.into_iter().zip(uniform).collect()
    };

    let mut pieces: Vec<Piece> = Vec::new();
    let mut regions: Vec<Region> = Vec::new();
    let mut residual_bytes = 0u64;
    let mut cursor = 0u64;
    for (d, byte) in declared {
        classify_gap(
            img,
            cursor,
            d.start - cursor,
            &mut regions,
            &mut pieces,
            &mut residual_bytes,
        )
        .map_err(nds_io)?;
        match byte {
            Some(byte) => regions.push(Region::Fill { byte, len: d.len }),
            None => {
                regions.push(Region::Piece(pieces.len()));
                pieces.push(Piece {
                    name: d.name,
                    start: d.start,
                    len: d.len,
                });
            }
        }
        cursor = d.start + d.len;
    }
    classify_gap(
        img,
        cursor,
        total_len - cursor,
        &mut regions,
        &mut pieces,
        &mut residual_bytes,
    )
    .map_err(nds_io)?;

    if residual_bytes > total_len / 4 && residual_bytes > RESIDUE_FLOOR {
        return Err(Refusal::ExcessResidue {
            residual: residual_bytes,
            total: total_len,
        }
        .into());
    }
    let fill_bytes = regions
        .iter()
        .map(|r| match r {
            Region::Fill { len, .. } => *len,
            _ => 0,
        })
        .sum();
    Ok(Layout {
        total_len,
        declared_sectors: vol.sectors,
        pieces,
        regions,
        file_count: vol.file_count,
        dir_count: vol.dir_count,
        empty_files: vol.empty_files,
        multi_extent_files: vol.multi_extent_files,
        uniform_files,
        fill_bytes,
        residual_bytes,
        coalesced,
    })
}

/// `Some(byte)` when `[start, start+len)` is one repeated byte value.
fn uniform_byte<R: Read + Seek>(
    img: &mut R,
    start: u64,
    len: u64,
    buf: &mut [u8],
) -> io::Result<Option<u8>> {
    img.seek(SeekFrom::Start(start))?;
    let mut remaining = len;
    let mut byte: Option<u8> = None;
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buf.len() as u64)).expect("bounded");
        let chunk = &mut buf[..want];
        img.read_exact(chunk)?;
        remaining -= want as u64;
        let b = *byte.get_or_insert(chunk[0]);
        if chunk.iter().any(|&x| x != b) {
            return Ok(None);
        }
    }
    Ok(byte)
}

/// `classify_gap` only reads; its refusal variants cannot fire here.
fn nds_io(e: crate::nds::NdsError) -> Iso9660Error {
    match e {
        crate::nds::NdsError::Io(e) => Iso9660Error::Io(e),
        crate::nds::NdsError::Refused(r) => {
            Iso9660Error::Refused(Refusal::Structure(r.to_string()))
        }
    }
}

/// Fixture builders for the gates: minimal ISO9660 volumes laid out
/// contiguously after the descriptors — a `VIDEO_TS` volume for the
/// D113 gates, an arbitrary tree for the D114 ones.
#[doc(hidden)]
pub mod synth {
    use std::collections::BTreeMap;

    use super::{ID, PVD_SECTOR, PVD_TYPE, SECTOR};

    fn record(out: &mut Vec<u8>, lba: u32, len: u32, flags: u8, name: &[u8]) {
        let rec_len = (33 + name.len() + (name.len() + 1) % 2) as u8;
        let start = out.len();
        out.push(rec_len);
        out.push(0);
        out.extend_from_slice(&lba.to_le_bytes());
        out.extend_from_slice(&lba.to_be_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&[0; 7]); // recording date
        out.push(flags);
        out.extend_from_slice(&[0, 0]); // unit size, interleave gap
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.push(name.len() as u8);
        out.extend_from_slice(name);
        while out.len() < start + usize::from(rec_len) {
            out.push(0);
        }
    }

    /// Build the volume: `files` are `(name, bytes)` under `/VIDEO_TS`.
    #[must_use]
    pub fn volume(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let paths: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(n, b)| (format!("/VIDEO_TS/{n}"), b.clone()))
            .collect();
        let refs: Vec<(&str, &[u8])> = paths
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_slice()))
            .collect();
        tree(&refs, &Spec::default())
    }

    /// Layout knobs for [`tree`].
    #[derive(Debug, Default, Clone)]
    pub struct Spec {
        /// Record this file (absolute path) as `n` extents of equal
        /// sector count (the last takes the remainder).
        pub split: Option<(&'static str, u32)>,
        /// Sectors of zero pad after the declared volume.
        pub tail_sectors: u64,
        /// Sectors of undeclared non-uniform bytes after the declared
        /// volume (residue the tree does not name).
        pub junk_sectors: u64,
        /// Zero-length files to record (absolute paths).
        pub empty: Vec<&'static str>,
    }

    /// Build a volume from absolute paths (`/A/B.BIN`): directories
    /// are created as needed, one sector each; files follow, sector
    /// aligned, in the given order. The declared volume space covers
    /// exactly the descriptors, tables and files; `spec.tail_sectors`
    /// and `spec.junk_sectors` follow it unlisted.
    #[must_use]
    pub fn tree(files: &[(&str, &[u8])], spec: &Spec) -> Vec<u8> {
        // Directories: root first, then every parent, parents before
        // children (BTreeMap keeps lexical order, which nests).
        let mut dirs: BTreeMap<String, Vec<(String, u32, u32, u8)>> = BTreeMap::new();
        dirs.insert(String::new(), Vec::new());
        let mut all: Vec<&str> = files.iter().map(|(p, _)| *p).collect();
        all.extend(spec.empty.iter().copied());
        for path in &all {
            let mut parent = String::new();
            for comp in path.trim_start_matches('/').split('/').collect::<Vec<_>>()
                [..path.trim_start_matches('/').split('/').count() - 1]
                .iter()
            {
                let child = format!("{parent}/{comp}");
                dirs.entry(child.clone()).or_default();
                parent = child;
            }
        }
        let dir_names: Vec<String> = dirs.keys().cloned().collect();
        let dir_lba: BTreeMap<String, u32> = dir_names
            .iter()
            .enumerate()
            .map(|(i, d)| (d.clone(), 18 + u32::try_from(i).expect("small")))
            .collect();
        let mut next = 18 + u32::try_from(dir_names.len()).expect("small");

        // Files: (path, lba, len, offset into the file's bytes) per extent.
        let mut placements: Vec<(String, u32, usize, usize)> = Vec::new();
        for (path, bytes) in files {
            let parent = path[..path.rfind('/').expect("absolute")].to_owned();
            let name = format!("{};1", &path[path.rfind('/').expect("absolute") + 1..]);
            let sectors = u32::try_from(bytes.len().div_ceil(SECTOR as usize)).expect("small");
            let extents: Vec<(u32, u32)> = match spec.split {
                Some((split_path, n)) if split_path == *path && n > 1 => {
                    let per = sectors / n;
                    let mut out = Vec::new();
                    let mut left = u32::try_from(bytes.len()).expect("small");
                    let mut lba = next;
                    for k in 0..n {
                        let len = if k + 1 == n {
                            left
                        } else {
                            (per * SECTOR as u32).min(left)
                        };
                        out.push((lba, len));
                        lba += len.div_ceil(SECTOR as u32);
                        left -= len;
                    }
                    out
                }
                _ => vec![(next, u32::try_from(bytes.len()).expect("small"))],
            };
            let n = extents.len();
            let mut offset = 0usize;
            for (k, (lba, len)) in extents.into_iter().enumerate() {
                let flags = if k + 1 < n { 0x80 } else { 0 };
                dirs.get_mut(&parent)
                    .expect("parent exists")
                    .push((name.clone(), lba, len, flags));
                placements.push((path.to_string(), lba, len as usize, offset));
                offset += len as usize;
            }
            next += sectors;
        }
        for path in &spec.empty {
            let parent = path[..path.rfind('/').expect("absolute")].to_owned();
            let name = format!("{};1", &path[path.rfind('/').expect("absolute") + 1..]);
            dirs.get_mut(&parent)
                .expect("parent exists")
                .push((name, next, 0, 0));
        }
        let total_sectors = next;
        let image_sectors = u64::from(total_sectors) + spec.tail_sectors + spec.junk_sectors;
        let mut out = vec![0u8; usize::try_from(image_sectors * SECTOR).expect("small")];

        let mut pvd = vec![0u8; 2048];
        pvd[0] = PVD_TYPE;
        pvd[1..6].copy_from_slice(ID);
        pvd[6] = 1;
        pvd[40..51].copy_from_slice(b"DATBOI_TEST");
        pvd[80..84].copy_from_slice(&total_sectors.to_le_bytes());
        pvd[84..88].copy_from_slice(&total_sectors.to_be_bytes());
        pvd[120..122].copy_from_slice(&1u16.to_le_bytes());
        pvd[122..124].copy_from_slice(&1u16.to_be_bytes());
        pvd[124..126].copy_from_slice(&1u16.to_le_bytes());
        pvd[126..128].copy_from_slice(&1u16.to_be_bytes());
        pvd[128..130].copy_from_slice(&2048u16.to_le_bytes());
        pvd[130..132].copy_from_slice(&2048u16.to_be_bytes());
        let root_lba = dir_lba[""];
        let mut root = Vec::new();
        record(&mut root, root_lba, 2048, 0x02, &[0]);
        pvd[156..156 + root.len()].copy_from_slice(&root);
        let at = usize::try_from(PVD_SECTOR * SECTOR).expect("small");
        out[at..at + 2048].copy_from_slice(&pvd);
        let mut term = vec![0u8; 2048];
        term[0] = 0xFF;
        term[1..6].copy_from_slice(ID);
        term[6] = 1;
        out[at + 2048..at + 4096].copy_from_slice(&term);

        for (dir, entries) in &dirs {
            let lba = dir_lba[dir];
            let parent_lba = dir.rfind('/').map_or(lba, |i| dir_lba[&dir[..i]]);
            let mut table = Vec::new();
            record(&mut table, lba, 2048, 0x02, &[0]);
            record(&mut table, parent_lba, 2048, 0x02, &[1]);
            for (child, child_lba) in &dir_lba {
                if child.rfind('/').is_some_and(|i| &child[..i] == dir) {
                    let name = &child[child.rfind('/').unwrap() + 1..];
                    record(&mut table, *child_lba, 2048, 0x02, name.as_bytes());
                }
            }
            for (name, lba, len, flags) in entries {
                record(&mut table, *lba, *len, *flags, name.as_bytes());
            }
            assert!(table.len() <= 2048, "test directory fits one sector");
            table.resize(2048, 0);
            let at = usize::try_from(u64::from(lba) * SECTOR).expect("small");
            out[at..at + 2048].copy_from_slice(&table);
        }

        for (path, lba, len, offset) in &placements {
            let bytes = files.iter().find(|(p, _)| p == path).expect("placed").1;
            let at = usize::try_from(u64::from(*lba) * SECTOR).expect("small");
            out[at..at + len].copy_from_slice(&bytes[*offset..*offset + len]);
        }
        if spec.junk_sectors > 0 {
            let from = usize::try_from((u64::from(total_sectors) + spec.tail_sectors) * SECTOR)
                .expect("small");
            for (i, b) in out[from..].iter_mut().enumerate() {
                *b = (i % 253) as u8 ^ 0x5A;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn walks_a_synthetic_dvd_video_volume() {
        let ifo = vec![0x11u8; 12288];
        let vob = (0..70_000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>();
        let bytes = synth::volume(&[("VIDEO_TS.IFO", ifo.clone()), ("VTS_01_1.VOB", vob.clone())]);
        let vol = parse_volume(&mut Cursor::new(&bytes), 0, bytes.len() as u64).expect("volume");
        assert_eq!(vol.sectors * SECTOR, bytes.len() as u64);
        assert_eq!(vol.file_count, 2);
        assert_eq!(vol.dir_count, 2);
        let paths: Vec<&str> = vol.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "/",
                "/VIDEO_TS",
                "/VIDEO_TS/VIDEO_TS.IFO",
                "/VIDEO_TS/VTS_01_1.VOB"
            ]
        );
        let vob_entry = &vol.entries[3];
        assert_eq!(vob_entry.len, vob.len() as u64);
        let at = usize::try_from(vob_entry.lba * SECTOR).expect("small");
        assert_eq!(&bytes[at..at + vob.len()], &vob[..]);
    }

    #[test]
    fn refuses_junk_and_extents_past_the_limit() {
        let junk = vec![0u8; 64 * 1024];
        assert!(matches!(
            parse_volume(&mut Cursor::new(&junk), 0, junk.len() as u64),
            Err(Iso9660Error::Refused(Refusal::NotIso9660))
        ));
        let bytes = synth::volume(&[("A.VOB", vec![1u8; 10_000])]);
        // A limit that cuts the file extent.
        assert!(matches!(
            parse_volume(&mut Cursor::new(&bytes), 0, 21 * SECTOR),
            Err(Iso9660Error::Refused(Refusal::Structure(_)))
        ));
    }
}
