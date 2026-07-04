//! Tiny shared helpers for the quick-xml pull parsers.

use quick_xml::Reader;
use quick_xml::events::BytesStart;
use quick_xml::events::attributes::Attribute;

use crate::model::ParseError;

/// Decode one attribute into owned (key, value) strings.
pub(crate) fn attr_pair(
    reader: &Reader<&[u8]>,
    attr: &Attribute<'_>,
) -> Result<(String, String), ParseError> {
    let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
    let value = attr
        .decode_and_unescape_value(reader.decoder())?
        .into_owned();
    Ok((key, value))
}

/// Consume an element's entire subtree (for elements we deliberately skip,
/// e.g. MAME emulation-only machine children).
pub(crate) fn skip_subtree(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
) -> Result<(), ParseError> {
    reader.read_to_end(start.to_end().name())?;
    Ok(())
}

/// Read the text content of a simple `<elem>text</elem>` element.
/// `read_text` yields raw text; entities must be unescaped here just as
/// `decode_and_unescape_value` does for attributes (`&amp;` → `&`).
pub(crate) fn element_text(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
) -> Result<String, ParseError> {
    let raw = reader.read_text(start.to_end().name())?;
    let unescaped = quick_xml::escape::unescape(&raw).map_err(quick_xml::Error::from)?;
    Ok(unescaped.into_owned())
}

pub(crate) fn pos(reader: &Reader<&[u8]>) -> u64 {
    reader.buffer_position()
}
