//! Dat family parsers and the header-skipper (detector) interpreter.
//!
//! Design record: docs/60-dats.md, decisions D9/D13. All families parse
//! losslessly into the canonical Entry/RomClaim model; unknown attributes
//! are preserved in attrs maps.

/// The dat families datboi accommodates (D13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatFormat {
    /// Logiqx `datafile` XML — the lingua franca (No-Intro, Redump, TOSEC…).
    Logiqx,
    /// clrmamepro paren-delimited text.
    ClrMamePro,
    /// RomCenter INI-ish (import-only).
    RomCenter,
    /// MAME `-listxml` machine dumps.
    MameListXml,
    /// MAME per-system software lists.
    MameSoftwareList,
}

/// Cheap format detection from the head of a dat file. Best-effort hint;
/// real parsers make the final call.
#[must_use]
pub fn detect(head: &[u8]) -> Option<DatFormat> {
    let text = String::from_utf8_lossy(&head[..head.len().min(4096)]);
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with("clrmamepro") || t.starts_with("emulator (") {
        return Some(DatFormat::ClrMamePro);
    }
    if t.starts_with("[CREDITS]") || t.starts_with("[DAT]") {
        return Some(DatFormat::RomCenter);
    }
    if t.starts_with("<?xml") || t.starts_with('<') {
        if t.contains("<mame") || t.contains("mame.dtd") {
            return Some(DatFormat::MameListXml);
        }
        if t.contains("<softwarelist") || t.contains("softwarelist.dtd") {
            return Some(DatFormat::MameSoftwareList);
        }
        if t.contains("<datafile") || t.contains("datafile.dtd") {
            return Some(DatFormat::Logiqx);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_families_from_heads() {
        assert_eq!(
            detect(b"<?xml version=\"1.0\"?>\n<!DOCTYPE datafile PUBLIC \"-//Logiqx//DTD ROM Management Datafile//EN\" \"http://www.logiqx.com/Dats/datafile.dtd\">"),
            Some(DatFormat::Logiqx)
        );
        assert_eq!(
            detect(b"clrmamepro (\n\tname \"Nintendo\"\n)"),
            Some(DatFormat::ClrMamePro)
        );
        assert_eq!(detect(b"[CREDITS]\nauthor=x"), Some(DatFormat::RomCenter));
        assert_eq!(
            detect(b"<?xml version=\"1.0\"?><mame build=\"0.270\">"),
            Some(DatFormat::MameListXml)
        );
        assert_eq!(
            detect(b"<softwarelist name=\"gba\">"),
            Some(DatFormat::MameSoftwareList)
        );
        assert_eq!(detect(b"NES\x1a"), None);
    }
}
