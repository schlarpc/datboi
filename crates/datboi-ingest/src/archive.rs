//! 7z / rar member extraction (M3 "7z/rar input").
//!
//! Unlike zip (whose members ride windowed builtin recipes and stay
//! claims), these formats have no D5-compliant rebuild transform yet —
//! LZMA-class recreation is future work — so ingest EXTRACTS members
//! into the CAS as first-class resident blobs with full alias tuples
//! (the bytes dats actually name), and the container stays a literal
//! (D24). Storage cost until an LZMA transform exists: container +
//! members both resident; the win is that audit sees the members and
//! they dedupe against every other source of the same bytes.
//!
//! rar is extraction-only by license and by design (the unrar library
//! cannot create archives), which matches the ingest direction exactly.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// 7z signature: `'7' 'z' 0xBC 0xAF 0x27 0x1C`.
#[must_use]
pub fn looks_like_7z(head: &[u8]) -> bool {
    head.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C])
}

/// rar4 (`Rar!\x1a\x07\x00`) or rar5 (`Rar!\x1a\x07\x01\x00`).
#[must_use]
pub fn looks_like_rar(head: &[u8]) -> bool {
    head.starts_with(b"Rar!\x1a\x07")
}

/// One extracted member, streamed into the store by the caller-supplied
/// sink (so this module stays store-agnostic and testable).
pub struct ExtractedMember {
    pub name: String,
    pub size: u64,
}

/// Walk a 7z container, handing each file member's decoded stream to
/// `sink(name, reader)`. Directories and anti-files are skipped.
///
/// # Errors
/// Unsupported codecs, encrypted archives, and corrupt streams surface
/// as one error string; the caller records it and the container stays
/// an opaque literal.
pub fn extract_7z(
    blob: &mut File,
    mut sink: impl FnMut(&str, &mut dyn Read) -> Result<(), String>,
) -> Result<Vec<ExtractedMember>, String> {
    blob.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut reader = sevenz_rust2::ArchiveReader::new(blob, sevenz_rust2::Password::empty())
        .map_err(|e| format!("7z open failed: {e}"))?;
    let mut members = Vec::new();
    reader
        .for_each_entries(|entry, reader| {
            if entry.is_directory() {
                return Ok(true);
            }
            sink(&entry.name, reader)
                .map_err(|e| sevenz_rust2::Error::Other(e.into()))?;
            members.push(ExtractedMember {
                name: entry.name.clone(),
                size: entry.size,
            });
            Ok(true)
        })
        .map_err(|e| format!("7z extraction failed: {e}"))?;
    Ok(members)
}

/// Walk a rar container at `path` (the unrar library opens by path, not
/// by reader), extracting each file member to `spool_dir` and handing
/// the spooled file to `sink`. The spool file is removed after each
/// member.
///
/// # Errors
/// As [`extract_7z`]; additionally multi-volume archives are refused.
pub fn extract_rar(
    path: &Path,
    spool_dir: &Path,
    mut sink: impl FnMut(&str, &mut File) -> Result<(), String>,
) -> Result<Vec<ExtractedMember>, String> {
    let mut members = Vec::new();
    let mut cursor = Some(
        unrar::Archive::new(path)
            .open_for_processing()
            .map_err(|e| format!("rar open failed: {e}"))?,
    );
    loop {
        let archive = cursor.take().expect("cursor present");
        let Some(header) = archive
            .read_header()
            .map_err(|e| format!("rar header read failed: {e}"))?
        else {
            break;
        };
        let entry = header.entry();
        let name = entry.filename.to_string_lossy().into_owned();
        let size = entry.unpacked_size;
        if entry.is_split() {
            return Err(format!(
                "member {name:?} spans volumes; multi-volume rar is unsupported"
            ));
        }
        cursor = Some(if entry.is_file() {
            let spool = spool_dir.join("rar-member.spool");
            let next = header
                .extract_to(&spool)
                .map_err(|e| format!("rar extract of {name:?} failed: {e}"))?;
            let mut file = File::open(&spool)
                .map_err(|e| format!("spooled member {name:?} unreadable: {e}"))?;
            let result = sink(&name, &mut file);
            let _ = std::fs::remove_file(&spool);
            result?;
            members.push(ExtractedMember { name, size });
            next
        } else {
            header
                .skip()
                .map_err(|e| format!("rar skip of {name:?} failed: {e}"))?
        });
    }
    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_detection() {
        assert!(looks_like_7z(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 4]));
        assert!(!looks_like_7z(b"PK\x03\x04"));
        assert!(looks_like_rar(b"Rar!\x1a\x07\x00rest"));
        assert!(looks_like_rar(b"Rar!\x1a\x07\x01\x00rest"));
        assert!(!looks_like_rar(b"Rar?"));
    }
}
