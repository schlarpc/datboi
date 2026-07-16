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
//! rar is extraction-only by license and by design (unrar cannot create
//! archives), which matches the ingest direction exactly. Since D58 the
//! rar path runs the `ex-unrar` component (unrar's C++ inside the wasm
//! sandbox) — see `Ingester::process_rar`; this module keeps only the 7z
//! extraction and the magic sniffs.

use std::io::{Read, Seek, SeekFrom};

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
pub fn extract_7z<R: Read + Seek>(
    blob: &mut R,
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
            sink(&entry.name, reader).map_err(|e| sevenz_rust2::Error::Other(e.into()))?;
            members.push(ExtractedMember {
                name: entry.name.clone(),
                size: entry.size,
            });
            Ok(true)
        })
        .map_err(|e| format!("7z extraction failed: {e}"))?;
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
