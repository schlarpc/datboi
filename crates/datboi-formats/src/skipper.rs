//! clrmamepro header-skipper (detector) parser + interpreter (D9).
//!
//! Spec: <https://mamedev.emulab.it/clrmamepro/docs/xmlheaders.txt>. A
//! detector holds ordered rules (OR'd, first fulfilled wins); a rule holds
//! ordered tests (AND'd) plus start/end offsets and an optional operation.
//! No rule fulfilled ⇒ whole file (we return `None`).
//!
//! Spec subtleties this implements exactly:
//! - Offsets are hex, max 64-bit; `-` prefix = relative to EOF; the literal
//!   `EOF` = file end. Applies to rule offsets AND test offsets.
//! - An *illegal seek/read* (test window outside the file, invalid rule
//!   range, operation size requirement violated) makes the rule fail
//!   outright — it is NOT a `false` test result, so `result="false"`
//!   cannot invert it into a pass.
//! - Operations (applied to the selected block before hashing):
//!   `bitswap` reverses bits per byte; `byteswap` swaps within 16-bit pairs
//!   (01|02 → 02|01, even length required); `wordswap` reverses each 32-bit
//!   group (01|02|03|04 → 04|03|02|01, length % 4 = 0); `wordbyteswap`
//!   swaps 16-bit words within each 32-bit group (01|02|03|04 → 03|04|01|02).
//! - `file size="PO2"` tests power-of-two size; `operator` is ignored for
//!   PO2 per spec.
//!
//! Recipe mapping (docs/recipes.md): a fulfilled decision is exactly
//! `assemble@1` (one blob-range segment `[start, end)`) when `operation` is
//! `none`, or that segment fed through `swap@1` with the matching mode
//! otherwise — so ingest can mint headered↔headerless recipes both
//! directions from a `SkipDecision` without re-reading the file.
//!
//! M1 note: evaluation takes a full slice; headers live at file heads, so
//! the ingest pipeline will hand this a bounded prefix window (plus tail
//! window for negative offsets) when streaming lands.

use std::str;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

/// A parsed detector XML: named, ordered rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detector {
    pub name: String,
    pub author: Option<String>,
    pub version: Option<String>,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub start: Offset,
    pub end: Offset,
    pub operation: Operation,
    pub tests: Vec<Test>,
}

/// A file offset expression: absolute, relative-to-EOF, or EOF itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offset {
    FromStart(u64),
    FromEnd(u64),
    Eof,
}

impl Offset {
    /// Resolve against a file length; `None` = illegal seek.
    fn resolve(self, len: u64) -> Option<u64> {
        match self {
            Self::FromStart(v) => (v <= len).then_some(v),
            Self::FromEnd(v) => len.checked_sub(v),
            Self::Eof => Some(len),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Operation {
    #[default]
    None,
    BitSwap,
    ByteSwap,
    WordSwap,
    WordByteSwap,
}

impl Operation {
    /// Block-length divisibility the spec requires for this operation.
    fn alignment(self) -> u64 {
        match self {
            Self::None | Self::BitSwap => 1,
            Self::ByteSwap => 2,
            Self::WordSwap | Self::WordByteSwap => 4,
        }
    }

    /// Apply in place. Callers guarantee `buf.len()` satisfies
    /// [`Operation::alignment`] (enforced by rule evaluation); a trailing
    /// remainder would be left untouched.
    pub fn apply(self, buf: &mut [u8]) {
        match self {
            Self::None => {}
            Self::BitSwap => {
                for b in buf {
                    *b = b.reverse_bits();
                }
            }
            Self::ByteSwap => {
                for pair in buf.chunks_exact_mut(2) {
                    pair.swap(0, 1);
                }
            }
            Self::WordSwap => {
                for quad in buf.chunks_exact_mut(4) {
                    quad.reverse();
                }
            }
            Self::WordByteSwap => {
                for quad in buf.chunks_exact_mut(4) {
                    quad.swap(0, 2);
                    quad.swap(1, 3);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Test {
    /// `<data>`: bytes at offset equal `value` (xor `expect` inversion).
    Data {
        offset: Offset,
        value: Vec<u8>,
        expect: bool,
    },
    /// `<and>`/`<or>`/`<xor>`: bytes at offset, masked byte-wise, equal `value`.
    Bool {
        op: BoolOp,
        offset: Offset,
        mask: Vec<u8>,
        value: Vec<u8>,
        expect: bool,
    },
    /// `<file>`: file-size comparison.
    File {
        size: SizeTest,
        operator: SizeOp,
        expect: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolOp {
    And,
    Or,
    Xor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeTest {
    Exact(u64),
    PowerOfTwo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SizeOp {
    #[default]
    Equal,
    Less,
    Greater,
}

/// The outcome of a fulfilled rule: real data is `[start, end)` of the
/// file, transformed by `operation` before hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkipDecision {
    pub start: u64,
    pub end: u64,
    pub operation: Operation,
}

impl SkipDecision {
    /// Length of the real data block.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// True when the decision selects the whole file unchanged (hashes
    /// would equal the plain file's).
    #[must_use]
    pub fn is_whole_file(&self, file_len: u64) -> bool {
        self.start == 0 && self.end == file_len && self.operation == Operation::None
    }

    /// Materialize the transformed real-data block.
    #[must_use]
    pub fn apply(&self, data: &[u8]) -> Vec<u8> {
        let start = usize::try_from(self.start).expect("start fits usize on supported targets");
        let end = usize::try_from(self.end).expect("end fits usize on supported targets");
        let mut block = data[start..end].to_vec();
        self.operation.apply(&mut block);
        block
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SkipperError {
    #[error("xml error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("xml attribute error: {0}")]
    Attr(#[from] quick_xml::events::attributes::AttrError),
    #[error("xml encoding error: {0}")]
    Encoding(#[from] quick_xml::encoding::EncodingError),
    #[error("detector is missing required <name>")]
    MissingName,
    #[error("detector has no <rule> elements")]
    NoRules,
    #[error("unknown element <{0}> (unknown tests cannot be treated as passing)")]
    UnknownElement(String),
    #[error("invalid {attr} attribute {value:?}")]
    InvalidAttr { attr: &'static str, value: String },
    #[error("<{element}> is missing required {attr} attribute")]
    MissingAttr {
        element: &'static str,
        attr: &'static str,
    },
    #[error("mask and value byte lengths differ")]
    MaskLength,
}

impl Detector {
    /// Parse a detector XML document.
    ///
    /// Unknown *attributes* are ignored (dats gotcha 8: tolerate and
    /// move on); unknown *elements* are errors — silently skipping a test
    /// we don't understand would change matching semantics.
    pub fn parse(xml: &[u8]) -> Result<Self, SkipperError> {
        let mut reader = Reader::from_reader(xml);
        let config = reader.config_mut();
        config.trim_text(true);
        config.expand_empty_elements = false;

        let mut name = None;
        let mut author = None;
        let mut version = None;
        let mut rules = Vec::new();
        let mut text_target: Option<&mut Option<String>> = None;

        loop {
            match reader.read_event()? {
                Event::Start(e) => match e.name().as_ref() {
                    b"detector" => {}
                    b"name" => text_target = Some(&mut name),
                    b"author" => text_target = Some(&mut author),
                    b"version" => text_target = Some(&mut version),
                    b"rule" => rules.push(parse_rule(&mut reader, &e, false)?),
                    other => {
                        return Err(SkipperError::UnknownElement(
                            String::from_utf8_lossy(other).into_owned(),
                        ));
                    }
                },
                Event::Empty(e) => match e.name().as_ref() {
                    b"rule" => rules.push(parse_rule(&mut reader, &e, true)?),
                    b"name" | b"author" | b"version" => {}
                    other => {
                        return Err(SkipperError::UnknownElement(
                            String::from_utf8_lossy(other).into_owned(),
                        ));
                    }
                },
                Event::Text(t) => {
                    if let Some(target) = text_target.take() {
                        *target = Some(t.decode()?.into_owned());
                    }
                }
                Event::End(_) => text_target = None,
                Event::Eof => break,
                _ => {}
            }
        }

        Ok(Self {
            name: name.ok_or(SkipperError::MissingName)?,
            author,
            version,
            rules: if rules.is_empty() {
                return Err(SkipperError::NoRules);
            } else {
                rules
            },
        })
    }

    /// Evaluate against file contents. `Some` = a rule was fulfilled;
    /// `None` = no rule matched, hash the whole file as-is.
    #[must_use]
    pub fn evaluate(&self, data: &[u8]) -> Option<SkipDecision> {
        self.rules.iter().find_map(|rule| rule.evaluate(data))
    }
}

impl Rule {
    fn evaluate(&self, data: &[u8]) -> Option<SkipDecision> {
        let len = data.len() as u64;
        let start = self.start.resolve(len)?;
        let end = self.end.resolve(len)?;
        if start > end {
            return None;
        }
        if !(end - start).is_multiple_of(self.operation.alignment()) {
            return None;
        }
        for test in &self.tests {
            if !test.passes(data)? {
                return None;
            }
        }
        Some(SkipDecision {
            start,
            end,
            operation: self.operation,
        })
    }
}

impl Test {
    /// `Some(bool)` = test evaluated (after `result` inversion);
    /// `None` = illegal seek/read, which fails the rule uninvertibly.
    fn passes(&self, data: &[u8]) -> Option<bool> {
        match self {
            Self::Data {
                offset,
                value,
                expect,
            } => {
                let window = read_at(data, *offset, value.len())?;
                Some((window == value.as_slice()) == *expect)
            }
            Self::Bool {
                op,
                offset,
                mask,
                value,
                expect,
            } => {
                let window = read_at(data, *offset, value.len())?;
                let matched = window.iter().zip(mask).zip(value).all(|((b, m), v)| {
                    let masked = match op {
                        BoolOp::And => b & m,
                        BoolOp::Or => b | m,
                        BoolOp::Xor => b ^ m,
                    };
                    masked == *v
                });
                Some(matched == *expect)
            }
            Self::File {
                size,
                operator,
                expect,
            } => {
                let len = data.len() as u64;
                let matched = match size {
                    // Operator is ignored for PO2 per spec.
                    SizeTest::PowerOfTwo => len.is_power_of_two(),
                    SizeTest::Exact(target) => match operator {
                        SizeOp::Equal => len == *target,
                        SizeOp::Less => len < *target,
                        SizeOp::Greater => len > *target,
                    },
                };
                Some(matched == *expect)
            }
        }
    }
}

fn read_at(data: &[u8], offset: Offset, len: usize) -> Option<&[u8]> {
    let start = usize::try_from(offset.resolve(data.len() as u64)?).ok()?;
    data.get(start..start.checked_add(len)?)
}

fn parse_rule(
    reader: &mut Reader<&[u8]>,
    start_tag: &BytesStart<'_>,
    is_empty: bool,
) -> Result<Rule, SkipperError> {
    let mut start = Offset::FromStart(0);
    let mut end = Offset::Eof;
    let mut operation = Operation::None;

    for attr in start_tag.attributes() {
        let attr = attr?;
        let value = attr.decode_and_unescape_value(reader.decoder())?;
        match attr.key.as_ref() {
            b"start_offset" => start = parse_offset("start_offset", &value)?,
            b"end_offset" => end = parse_offset("end_offset", &value)?,
            b"operation" => {
                operation = match value.as_ref() {
                    "none" => Operation::None,
                    "bitswap" => Operation::BitSwap,
                    "byteswap" => Operation::ByteSwap,
                    "wordswap" => Operation::WordSwap,
                    "wordbyteswap" => Operation::WordByteSwap,
                    other => {
                        return Err(SkipperError::InvalidAttr {
                            attr: "operation",
                            value: other.to_owned(),
                        });
                    }
                }
            }
            _ => {} // unknown attributes tolerated
        }
    }

    let mut tests = Vec::new();
    if !is_empty {
        loop {
            match reader.read_event()? {
                Event::Empty(e) | Event::Start(e) => {
                    tests.push(parse_test(reader, &e)?);
                }
                Event::End(e) if e.name().as_ref() == b"rule" => break,
                Event::End(_) | Event::Text(_) | Event::Comment(_) => {}
                Event::Eof => break,
                _ => {}
            }
        }
    }

    Ok(Rule {
        start,
        end,
        operation,
        tests,
    })
}

fn parse_test(reader: &Reader<&[u8]>, tag: &BytesStart<'_>) -> Result<Test, SkipperError> {
    let element = match tag.name().as_ref() {
        b"data" => "data",
        b"and" => "and",
        b"or" => "or",
        b"xor" => "xor",
        b"file" => "file",
        other => {
            return Err(SkipperError::UnknownElement(
                String::from_utf8_lossy(other).into_owned(),
            ));
        }
    };

    let mut offset = Offset::FromStart(0);
    let mut value = None;
    let mut mask = None;
    let mut size = None;
    let mut operator = SizeOp::Equal;
    let mut expect = true;

    for attr in tag.attributes() {
        let attr = attr?;
        let raw = attr.decode_and_unescape_value(reader.decoder())?;
        match attr.key.as_ref() {
            b"offset" => offset = parse_offset("offset", &raw)?,
            b"value" => value = Some(parse_hex_bytes("value", &raw)?),
            b"mask" => mask = Some(parse_hex_bytes("mask", &raw)?),
            b"size" => {
                size = Some(if raw.eq_ignore_ascii_case("PO2") {
                    SizeTest::PowerOfTwo
                } else {
                    SizeTest::Exact(parse_hex_u64("size", &raw)?)
                });
            }
            b"operator" => {
                operator = match raw.as_ref() {
                    "equal" => SizeOp::Equal,
                    "less" => SizeOp::Less,
                    "greater" => SizeOp::Greater,
                    other => {
                        return Err(SkipperError::InvalidAttr {
                            attr: "operator",
                            value: other.to_owned(),
                        });
                    }
                }
            }
            b"result" => {
                expect = match raw.as_ref() {
                    "true" => true,
                    "false" => false,
                    other => {
                        return Err(SkipperError::InvalidAttr {
                            attr: "result",
                            value: other.to_owned(),
                        });
                    }
                }
            }
            _ => {} // unknown attributes tolerated
        }
    }

    match element {
        "file" => Ok(Test::File {
            size: size.ok_or(SkipperError::MissingAttr {
                element: "file",
                attr: "size",
            })?,
            operator,
            expect,
        }),
        "data" => {
            let value = value.ok_or(SkipperError::MissingAttr {
                element: "data",
                attr: "value",
            })?;
            if value.is_empty() {
                return Err(SkipperError::InvalidAttr {
                    attr: "value",
                    value: String::new(),
                });
            }
            Ok(Test::Data {
                offset,
                value,
                expect,
            })
        }
        bool_element => {
            let op = match bool_element {
                "and" => BoolOp::And,
                "or" => BoolOp::Or,
                _ => BoolOp::Xor,
            };
            let value = value.ok_or(SkipperError::MissingAttr {
                element: "boolean test",
                attr: "value",
            })?;
            let mask = mask.ok_or(SkipperError::MissingAttr {
                element: "boolean test",
                attr: "mask",
            })?;
            if mask.len() != value.len() {
                return Err(SkipperError::MaskLength);
            }
            Ok(Test::Bool {
                op,
                offset,
                mask,
                value,
                expect,
            })
        }
    }
}

fn parse_offset(attr: &'static str, raw: &str) -> Result<Offset, SkipperError> {
    if raw.eq_ignore_ascii_case("EOF") {
        return Ok(Offset::Eof);
    }
    if let Some(neg) = raw.strip_prefix('-') {
        return Ok(Offset::FromEnd(parse_hex_u64(attr, neg)?));
    }
    Ok(Offset::FromStart(parse_hex_u64(attr, raw)?))
}

fn parse_hex_u64(attr: &'static str, raw: &str) -> Result<u64, SkipperError> {
    u64::from_str_radix(raw, 16).map_err(|_| SkipperError::InvalidAttr {
        attr,
        value: raw.to_owned(),
    })
}

fn parse_hex_bytes(attr: &'static str, raw: &str) -> Result<Vec<u8>, SkipperError> {
    if !raw.len().is_multiple_of(2) {
        return Err(SkipperError::InvalidAttr {
            attr,
            value: raw.to_owned(),
        });
    }
    (0..raw.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&raw[i..i + 2], 16).map_err(|_| SkipperError::InvalidAttr {
                attr,
                value: raw.to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector(rules_xml: &str) -> Detector {
        let xml = format!("<?xml version=\"1.0\"?><detector><name>t</name>{rules_xml}</detector>");
        Detector::parse(xml.as_bytes()).expect("fixture parses")
    }

    #[test]
    fn operations_match_spec_examples() {
        // Spec: byteswap 01|02 -> 02|01; wordswap 01|02|03|04 -> 04|03|02|01;
        // wordbyteswap 01|02|03|04 -> 03|04|01|02.
        let mut b = vec![0x01, 0x02];
        Operation::ByteSwap.apply(&mut b);
        assert_eq!(b, [0x02, 0x01]);

        let mut w = vec![0x01, 0x02, 0x03, 0x04];
        Operation::WordSwap.apply(&mut w);
        assert_eq!(w, [0x04, 0x03, 0x02, 0x01]);

        let mut wb = vec![0x01, 0x02, 0x03, 0x04];
        Operation::WordByteSwap.apply(&mut wb);
        assert_eq!(wb, [0x03, 0x04, 0x01, 0x02]);

        let mut bits = vec![0b1000_0001, 0x0F];
        Operation::BitSwap.apply(&mut bits);
        assert_eq!(bits, [0b1000_0001, 0xF0]);
    }

    #[test]
    fn defaults_and_untested_rule_always_match() {
        // "A rule without any tests is always 'fulfilled'."
        let d = detector(r#"<rule start_offset="80"/>"#);
        let data = vec![0u8; 0x100];
        assert_eq!(
            d.evaluate(&data),
            Some(SkipDecision {
                start: 0x80,
                end: 0x100,
                operation: Operation::None
            })
        );
    }

    #[test]
    fn negative_offsets_and_boolean_tests() {
        // Spec example 2: start 0x10, end EOF-0x40, AND-mask test at EOF-2.
        let d = detector(
            r#"<rule start_offset="10" end_offset="-40">
                 <and offset="-2" mask="f0" value="20" result="false"/>
               </rule>"#,
        );
        let mut data = vec![0u8; 0x100];
        data[0x100 - 2] = 0x30; // 0x30 & 0xf0 = 0x30 != 0x20 -> test true
        assert_eq!(
            d.evaluate(&data),
            Some(SkipDecision {
                start: 0x10,
                end: 0xC0,
                operation: Operation::None
            })
        );
        data[0x100 - 2] = 0x2F; // 0x2F & 0xf0 = 0x20 -> inverted -> rule fails
        assert_eq!(d.evaluate(&data), None);
    }

    #[test]
    fn po2_file_test_and_rule_ordering() {
        // Spec example 1: skip 0x80 when magic present AND size not PO2;
        // else whole file when size is PO2.
        let d = detector(
            r#"<rule start_offset="80" end_offset="EOF">
                 <data offset="64" value="41435455414C2043" result="true"/>
                 <file size="PO2" result="false"/>
               </rule>
               <rule start_offset="0" end_offset="EOF">
                 <file size="PO2" result="true"/>
               </rule>"#,
        );
        let mut headered = vec![0u8; 0x180]; // not PO2
        headered[0x64..0x6C].copy_from_slice(b"ACTUAL C");
        assert_eq!(d.evaluate(&headered).unwrap().start, 0x80);

        let plain = vec![0u8; 0x200]; // PO2: second rule
        let decision = d.evaluate(&plain).unwrap();
        assert!(decision.is_whole_file(0x200));

        let neither = vec![0u8; 0x180]; // no magic, not PO2
        assert_eq!(d.evaluate(&neither), None);
    }

    #[test]
    fn illegal_reads_are_not_invertible() {
        // A data test past EOF must fail the rule even with result="false" —
        // "illegal seeks/reads" are not test outcomes.
        let d = detector(
            r#"<rule start_offset="10">
                 <data offset="1000" value="41" result="false"/>
               </rule>"#,
        );
        assert_eq!(d.evaluate(&[0u8; 16]), None);
    }

    #[test]
    fn operation_alignment_gates_the_rule() {
        // byteswap requires an even block; odd file -> rule not fulfilled.
        let d = detector(r#"<rule operation="byteswap"/>"#);
        assert_eq!(d.evaluate(&[0u8; 5]), None);
        let decision = d.evaluate(&[0u8; 6]).unwrap();
        assert_eq!(decision.operation, Operation::ByteSwap);
        assert_eq!(decision.apply(&[1, 2, 3, 4, 5, 6]), [2, 1, 4, 3, 6, 5]);
    }

    #[test]
    fn rejects_unknown_test_elements_and_missing_name() {
        let bad = r#"<detector><name>x</name><rule><frobnicate/></rule></detector>"#;
        assert!(matches!(
            Detector::parse(bad.as_bytes()),
            Err(SkipperError::UnknownElement(e)) if e == "frobnicate"
        ));
        let unnamed = r#"<detector><rule/></detector>"#;
        assert!(matches!(
            Detector::parse(unnamed.as_bytes()),
            Err(SkipperError::MissingName)
        ));
    }

    #[test]
    fn tolerates_unknown_attributes() {
        let d = detector(r#"<rule start_offset="10" future_attr="yes"/>"#);
        assert_eq!(d.rules.len(), 1);
    }
}
