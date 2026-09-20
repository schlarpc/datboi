//! Dat family parsers and the header-skipper (detector) interpreter.
//!
//! Design record: docs/dats.md, decisions D9/D13. All families parse
//! losslessly into the canonical Entry/RomClaim model; unknown attributes
//! are preserved in attrs maps.

pub mod chd;
pub mod cmpro;
pub mod listxml;
pub mod logiqx;
pub mod model;
pub mod skipper;
pub mod softlist;
mod xmlutil;

/// Parse any supported dat family, dispatching on [`detect`].
///
/// # Errors
/// [`model::ParseError::UnknownFormat`] when detection fails;
/// [`model::ParseError::Unsupported`] for recognized-but-unimplemented
/// families (RomCenter).
pub fn parse(bytes: &[u8]) -> Result<model::DatFile, model::ParseError> {
    match detect(bytes) {
        Some(DatFormat::Logiqx) => logiqx::parse(bytes),
        Some(DatFormat::MameListXml) => listxml::parse(bytes),
        Some(DatFormat::MameSoftwareList) => softlist::parse(bytes),
        Some(DatFormat::ClrMamePro) => cmpro::parse(bytes),
        Some(DatFormat::RomCenter) => Err(model::ParseError::Unsupported(
            "romcenter (import planned, not yet implemented)",
        )),
        None => Err(model::ParseError::UnknownFormat),
    }
}

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
        // The DOCTYPE checks are not redundant with the element ones. MAME
        // emits its DTD *inline* rather than as a system identifier, and
        // that internal subset is ~7 KB of ELEMENT/ATTLIST declarations —
        // so on a real `mame -listxml` the root `<mame build=...>` sits
        // well past this window and "mame.dtd" never appears at all.
        // The DOCTYPE is the first thing after the XML declaration either
        // way, so matching it makes the window size stop mattering.
        if t.contains("<!DOCTYPE mame") || t.contains("<mame") || t.contains("mame.dtd") {
            return Some(DatFormat::MameListXml);
        }
        if t.contains("<!DOCTYPE softwarelist")
            || t.contains("<softwarelist")
            || t.contains("softwarelist.dtd")
        {
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

    /// A real `mame -listxml` opens with an inline DTD, so neither the
    /// root element nor a system identifier is anywhere near the head of
    /// the file: 0.287's internal subset runs ~7 KB before `<mame build=`.
    /// Detection has to land on the DOCTYPE alone.
    #[test]
    fn detects_mame_listxml_behind_an_inline_dtd() {
        let mut head = String::from("<?xml version=\"1.0\"?>\n<!DOCTYPE mame [\n");
        head.push_str("<!ELEMENT mame (machine+)>\n");
        // Pad past the 4096-byte sniff window the way the real DTD does.
        for _ in 0..200 {
            head.push_str("\t<!ATTLIST machine sourcefile CDATA #IMPLIED>\n");
        }
        head.push_str("]>\n<mame build=\"0.287\">");
        assert!(head.len() > 4096);
        assert_eq!(detect(head.as_bytes()), Some(DatFormat::MameListXml));

        let soft = "<?xml version=\"1.0\"?>\n<!DOCTYPE softwarelist [\n<!ELEMENT softwarelist (software+)>\n]>";
        assert_eq!(detect(soft.as_bytes()), Some(DatFormat::MameSoftwareList));
    }
}
