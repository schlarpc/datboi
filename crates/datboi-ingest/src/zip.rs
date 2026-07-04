//! Minimal zip central-directory reader — just enough to claim members
//! (docs/90-roadmap.md: containers stay literal, members are claims).
//!
//! Owned rather than the `zip` crate: we need exact raw byte ranges
//! (local-header data offsets) to mint slice/deflate recipes over the
//! stored container, and the format subset we accept is deliberately
//! small: single-disk, non-zip64, STORED or DEFLATE, unencrypted. zip64
//! and other methods are reported, never guessed at (M1 scope; the wild
//! rom world is overwhelmingly TorrentZip-shaped, which fits this subset).
//!
//! Sizes and the data offset come from the central directory + local
//! header; general-purpose bit 3 (data descriptor) does not affect either,
//! so descriptor-bearing zips work unmodified.

use std::io::{Read, Seek, SeekFrom};

const EOCD_SIG: u32 = 0x0605_4b50;
const CD_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;
/// EOCD fixed size + maximum comment length.
const EOCD_SCAN_WINDOW: u64 = 22 + 65_535;

#[derive(Debug, thiserror::Error)]
pub enum ZipError {
    #[error("zip i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("no end-of-central-directory record")]
    NoEocd,
    #[error("zip64 archives are not supported in M1")]
    Zip64Unsupported,
    #[error("malformed central directory: {0}")]
    BadCentralDirectory(&'static str),
    #[error("malformed local header for member {0:?}")]
    BadLocalHeader(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Stored,
    Deflate,
}

/// A member we can claim: its raw data lives at
/// `[data_start, data_start + comp_size)` of the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub method: Method,
    pub comp_size: u64,
    pub uncomp_size: u64,
    pub data_start: u64,
}

/// A member present in the archive but outside the M1 subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedMember {
    pub name: String,
    pub reason: &'static str,
}

pub struct Parsed {
    pub members: Vec<Member>,
    pub skipped: Vec<SkippedMember>,
}

/// True if the first bytes look like a zip (local entry or empty archive).
#[must_use]
pub fn looks_like_zip(head: &[u8]) -> bool {
    head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06")
}

/// Parse the central directory of `file` (any `Read + Seek`, typically the
/// stored container blob). Directory entries are dropped silently; members
/// outside the subset land in `skipped`.
pub fn parse_members<R: Read + Seek>(file: &mut R) -> Result<Parsed, ZipError> {
    let file_len = file.seek(SeekFrom::End(0))?;
    let (cd_offset, cd_size, entries) = find_eocd(file, file_len)?;
    if u64::from(cd_offset) + u64::from(cd_size) > file_len {
        return Err(ZipError::BadCentralDirectory(
            "central directory extends past end of file",
        ));
    }

    file.seek(SeekFrom::Start(u64::from(cd_offset)))?;
    let mut cd = vec![0u8; cd_size as usize];
    file.read_exact(&mut cd)?;

    let mut members = Vec::new();
    let mut skipped = Vec::new();
    let mut pos = 0usize;
    for _ in 0..entries {
        let fixed = cd
            .get(pos..pos + 46)
            .ok_or(ZipError::BadCentralDirectory("truncated entry"))?;
        if u32_at(fixed, 0) != CD_SIG {
            return Err(ZipError::BadCentralDirectory("bad entry signature"));
        }
        let flags = u16_at(fixed, 8);
        let method = u16_at(fixed, 10);
        let comp_size = u32_at(fixed, 20);
        let uncomp_size = u32_at(fixed, 24);
        let name_len = u16_at(fixed, 28) as usize;
        let extra_len = u16_at(fixed, 30) as usize;
        let comment_len = u16_at(fixed, 32) as usize;
        let disk_start = u16_at(fixed, 34);
        let local_offset = u32_at(fixed, 42);
        let name_bytes = cd
            .get(pos + 46..pos + 46 + name_len)
            .ok_or(ZipError::BadCentralDirectory("truncated name"))?;
        // Deterministic regardless of the CP437/UTF-8 flag: claims need a
        // stable label, not a faithful filesystem round-trip.
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        pos += 46 + name_len + extra_len + comment_len;

        if name.ends_with('/') && uncomp_size == 0 {
            continue; // directory entry
        }
        let reason = if [comp_size, uncomp_size, local_offset].contains(&u32::MAX) {
            Some("zip64 member")
        } else if disk_start != 0 {
            Some("multi-disk member")
        } else if flags & 0x0001 != 0 {
            Some("encrypted")
        } else if method != 0 && method != 8 {
            Some("unsupported compression method")
        } else {
            None
        };
        if let Some(reason) = reason {
            skipped.push(SkippedMember { name, reason });
            continue;
        }

        let data_start = member_data_start(file, u64::from(local_offset), &name)?;
        if data_start + u64::from(comp_size) > file_len {
            return Err(ZipError::BadCentralDirectory(
                "member data extends past end of file",
            ));
        }
        members.push(Member {
            name,
            method: if method == 0 {
                Method::Stored
            } else {
                Method::Deflate
            },
            comp_size: u64::from(comp_size),
            uncomp_size: u64::from(uncomp_size),
            data_start,
        });
    }
    Ok(Parsed { members, skipped })
}

/// Locate the EOCD record: scan the tail window backwards for the last
/// signature whose comment length exactly reaches end-of-file.
fn find_eocd<R: Read + Seek>(file: &mut R, file_len: u64) -> Result<(u32, u32, u16), ZipError> {
    let window = file_len.min(EOCD_SCAN_WINDOW);
    file.seek(SeekFrom::Start(file_len - window))?;
    let mut tail = vec![0u8; window as usize];
    file.read_exact(&mut tail)?;

    let mut i = tail.len().checked_sub(22).ok_or(ZipError::NoEocd)?;
    loop {
        if u32_at(&tail, i) == EOCD_SIG {
            let comment_len = u16_at(&tail, i + 20) as usize;
            if i + 22 + comment_len == tail.len() {
                let disk_no = u16_at(&tail, i + 4);
                let cd_disk = u16_at(&tail, i + 6);
                let entries_here = u16_at(&tail, i + 8);
                let entries_total = u16_at(&tail, i + 10);
                let cd_size = u32_at(&tail, i + 12);
                let cd_offset = u32_at(&tail, i + 16);
                if entries_total == u16::MAX || cd_size == u32::MAX || cd_offset == u32::MAX {
                    return Err(ZipError::Zip64Unsupported);
                }
                if disk_no != 0 || cd_disk != 0 || entries_here != entries_total {
                    return Err(ZipError::BadCentralDirectory("multi-disk archive"));
                }
                return Ok((cd_offset, cd_size, entries_total));
            }
        }
        i = match i.checked_sub(1) {
            Some(i) => i,
            None => return Err(ZipError::NoEocd),
        };
    }
}

/// The local header's own name/extra lengths (which may differ from the
/// central directory's) decide where member data actually starts.
fn member_data_start<R: Read + Seek>(
    file: &mut R,
    local_offset: u64,
    name: &str,
) -> Result<u64, ZipError> {
    file.seek(SeekFrom::Start(local_offset))?;
    let mut header = [0u8; 30];
    file.read_exact(&mut header)
        .map_err(|_| ZipError::BadLocalHeader(name.to_owned()))?;
    if u32_at(&header, 0) != LOCAL_SIG {
        return Err(ZipError::BadLocalHeader(name.to_owned()));
    }
    let name_len = u64::from(u16_at(&header, 26));
    let extra_len = u64::from(u16_at(&header, 28));
    Ok(local_offset + 30 + name_len + extra_len)
}

fn u16_at(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}
