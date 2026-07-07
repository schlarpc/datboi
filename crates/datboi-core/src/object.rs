//! Structured-object typing (D18): raw data blobs are unwrapped and untyped;
//! datboi's own objects self-identify with a magic prefix line inside their
//! genuine content, followed by strict canonical CBOR.

/// Kinds of datboi structured objects. Raw data blobs have no kind — type
/// lives in edges, not nodes (D18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Recipe,
    ViewSnapshot,
    StateSnapshot,
    AliasBatch,
    /// Analyzer-provenance batch (D48): sharded rows of
    /// bytes × analyzer → outcome, referenced from state snapshots.
    AnalysisBatch,
}

impl ObjectKind {
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Recipe => "recipe",
            Self::ViewSnapshot => "viewsnap",
            Self::StateSnapshot => "statesnap",
            Self::AliasBatch => "aliases",
            Self::AnalysisBatch => "analysis",
        }
    }
}

/// Parse a structured-object header: `datboi/<tag>/<version>\n`.
///
/// Returns the kind, format version, and the offset where the CBOR body
/// begins. Used by recovery scans over `meta/` and as defense-in-depth
/// sniffing anywhere else (D20).
#[must_use]
pub fn sniff(bytes: &[u8]) -> Option<(ObjectKind, u32, usize)> {
    let prefix = b"datboi/";
    let rest = bytes.strip_prefix(prefix)?;
    let newline = rest.iter().position(|&b| b == b'\n')?;
    let line = std::str::from_utf8(&rest[..newline]).ok()?;
    let (tag, version) = line.split_once('/')?;
    let kind = match tag {
        "recipe" => ObjectKind::Recipe,
        "viewsnap" => ObjectKind::ViewSnapshot,
        "statesnap" => ObjectKind::StateSnapshot,
        "aliases" => ObjectKind::AliasBatch,
        "analysis" => ObjectKind::AnalysisBatch,
        _ => return None,
    };
    // Strict: version is a bare decimal integer, no leading zeros or signs.
    if version.is_empty()
        || version.len() > 1 && version.starts_with('0')
        || !version.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let version: u32 = version.parse().ok()?;
    Some((kind, version, prefix.len() + newline + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_recipe_header() {
        let obj = b"datboi/recipe/1\n\xa1\x01\x02";
        assert_eq!(sniff(obj), Some((ObjectKind::Recipe, 1, 16)));
    }

    #[test]
    fn rejects_non_objects() {
        assert_eq!(sniff(b"NES\x1a"), None); // an iNES rom is not a structured object
        assert_eq!(sniff(b"datboi/recipe/01\n"), None); // no leading zeros
        assert_eq!(sniff(b"datboi/mystery/1\n"), None); // unknown tag
        assert_eq!(sniff(b"datboi/recipe/1"), None); // missing newline
    }
}
