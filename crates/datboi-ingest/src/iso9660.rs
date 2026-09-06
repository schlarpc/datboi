//! ISO9660 volume reader (D113): the redump Xbox image's video
//! partition is a plain ECMA-119 DVD-Video volume, and this walks it —
//! primary volume descriptor at sector 16, directory records in tree
//! order — into file and directory extents so the `xdvdfs-split/1`
//! analyzer can claim them as named pieces and the declared volume as
//! a view. Deliberately minimal: no Joliet, no Rock Ridge, no UDF (the
//! UDF descriptors on a DVD-Video disc reference the same extents the
//! ISO9660 tree does; walking both would double-claim). Interleaved and
//! multi-extent files are refused — nothing on these discs uses them.
//!
//! Structural anomalies are deterministic conclusions ([`Refusal`]),
//! never environmental errors (D81); a wrong map fails D4 replay, the
//! image stays literal.

use std::collections::HashSet;
use std::io::{self, Read, Seek};

use crate::nds::{read_at, u32_at};

pub const SECTOR: u64 = 2048;
/// The primary volume descriptor's sector.
pub const PVD_SECTOR: u64 = 16;
const PVD_TYPE: u8 = 1;
const ID: &[u8; 5] = b"CD001";
const FLAG_DIRECTORY: u8 = 0x02;
const FLAG_MULTI_EXTENT: u8 = 0x80;
const MAX_DEPTH: usize = 32;
const MAX_ENTRIES: usize = 1 << 16;
/// A directory extent above this is a lie.
const MAX_DIR_EXTENT: u64 = 16 << 20;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("no iso9660 primary volume descriptor at sector 16")]
    NotIso9660,
    #[error("iso9660: {0}")]
    Structure(String),
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
    /// Files and directory tables, in tree order (root table first).
    pub entries: Vec<Entry>,
    pub file_count: usize,
    pub dir_count: usize,
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

    let mut entries: Vec<Entry> = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut file_count = 0usize;
    let mut dir_count = 0usize;
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
        if len == 0 || len > MAX_DIR_EXTENT || !len.is_multiple_of(SECTOR) {
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
            if flags & FLAG_MULTI_EXTENT != 0 {
                return Err(refuse("multi-extent file".into()).into());
            }
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
                stack.push((ext_lba, ext_len, full, depth + 1));
            } else {
                if ext_len == 0 {
                    continue;
                }
                if ext_lba * SECTOR + ext_len > limit {
                    return Err(refuse(format!("{full} extends past the partition")).into());
                }
                file_count += 1;
                entries.push(Entry {
                    path: full,
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
    })
}

/// Fixture builder for the gates (D113): a minimal ISO9660 volume with
/// a `VIDEO_TS` directory holding the given files, laid out
/// contiguously after the descriptors. Returns the volume bytes; the
/// declared volume space size covers exactly the bytes returned.
#[doc(hidden)]
pub mod synth {
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
        // Sector map: 0..16 system area, 16 PVD, 17 terminator,
        // 18 root dir, 19 VIDEO_TS dir, 20.. files.
        let root_lba = 18u32;
        let vts_lba = 19u32;
        let mut file_lbas = Vec::new();
        let mut next = 20u32;
        for (_, bytes) in files {
            file_lbas.push(next);
            next += u32::try_from(bytes.len().div_ceil(SECTOR as usize)).expect("small");
        }
        let total_sectors = next;
        let mut out = vec![0u8; usize::try_from(u64::from(total_sectors) * SECTOR).expect("small")];

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

        let mut root_table = Vec::new();
        record(&mut root_table, root_lba, 2048, 0x02, &[0]);
        record(&mut root_table, root_lba, 2048, 0x02, &[1]);
        record(&mut root_table, vts_lba, 2048, 0x02, b"VIDEO_TS");
        root_table.resize(2048, 0);
        let at = usize::try_from(u64::from(root_lba) * SECTOR).expect("small");
        out[at..at + 2048].copy_from_slice(&root_table);

        let mut vts_table = Vec::new();
        record(&mut vts_table, vts_lba, 2048, 0x02, &[0]);
        record(&mut vts_table, root_lba, 2048, 0x02, &[1]);
        for ((name, bytes), lba) in files.iter().zip(&file_lbas) {
            let versioned = format!("{name};1");
            record(
                &mut vts_table,
                *lba,
                u32::try_from(bytes.len()).expect("small"),
                0,
                versioned.as_bytes(),
            );
        }
        vts_table.resize(2048, 0);
        let at = usize::try_from(u64::from(vts_lba) * SECTOR).expect("small");
        out[at..at + 2048].copy_from_slice(&vts_table);

        for ((_, bytes), lba) in files.iter().zip(&file_lbas) {
            let at = usize::try_from(u64::from(*lba) * SECTOR).expect("small");
            out[at..at + bytes.len()].copy_from_slice(bytes);
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
