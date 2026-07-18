//! 7z / rar container magic sniffs (M3 "7z/rar input").
//!
//! Unlike zip (whose members ride windowed builtin recipes and stay
//! claims), these formats have no recompressor, so ingest EXTRACTS
//! members into the CAS as first-class resident blobs with full alias
//! tuples (the bytes dats actually name), and the container stays a
//! literal (D24). Since D58 (rar) and D110 (7z) both formats run
//! sandboxed extractor components — see `Ingester::process_rar` /
//! `Ingester::process_7z` — and members carry container→member derive
//! recipes, so they are evictable and the double-residency is
//! transient, not structural. The in-process sevenz-rust2 reader this
//! module once held left with D110 (its writer half survives as a
//! dev-dependency: the tests forge fixtures with it).

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
