//! MAME software-list parser. The part/dataarea/loadflag structure has no
//! Logiqx equivalent and is modeled losslessly (D13): parts hold structure,
//! claims stay flat for the uniform audit view, and the two share rows via
//! indices ([`crate::model::DataArea::claims`]).
//!
//! Wild-format notes: dataarea sizes appear both decimal and `0x`-hex;
//! `rom` rows may be nameless load directives (`loadflag="continue"` /
//! `"ignore"`); offsets are preserved verbatim in attrs (some lists write
//! bare hex without `0x`, so numeric interpretation is deferred to the
//! consumer that knows the loadflag semantics — post-MVP per D31).

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::logiqx::require_hash;
use crate::model::{
    ClaimKind, ClaimStatus, DatFile, DataArea, DiskArea, Entry, ParseError, RomClaim, SoftwarePart,
    parse_crc32, parse_sha1, parse_size,
};
use crate::xmlutil::{attr_pair, element_text, pos, skip_subtree};

pub fn parse(xml: &[u8]) -> Result<DatFile, ParseError> {
    let mut reader = Reader::from_reader(xml);
    let config = reader.config_mut();
    config.trim_text(true);
    config.expand_empty_elements = false;

    let mut dat = DatFile::default();
    let mut saw_root = false;
    loop {
        match reader.read_event()? {
            Event::Start(e) => match e.name().as_ref() {
                b"softwarelist" => {
                    saw_root = true;
                    for attr in e.attributes() {
                        let (key, value) = attr_pair(&reader, &attr?)?;
                        match key.as_str() {
                            "name" => dat.header.name = Some(value),
                            "description" => dat.header.description = Some(value),
                            _ => {
                                dat.header.attrs.insert(key, value);
                            }
                        }
                    }
                }
                b"software" => dat.entries.push(parse_software(&mut reader, &e)?),
                _ => skip_subtree(&mut reader, &e)?,
            },
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_root {
        return Err(ParseError::invalid(pos(&reader), "no <softwarelist> root"));
    }
    Ok(dat)
}

fn parse_software(reader: &mut Reader<&[u8]>, start: &BytesStart<'_>) -> Result<Entry, ParseError> {
    let mut entry = Entry::default();
    let mut name = None;
    for attr in start.attributes() {
        let (key, value) = attr_pair(reader, &attr?)?;
        match key.as_str() {
            "name" => name = Some(value),
            "cloneof" => entry.cloneof = Some(value),
            _ => {
                entry.attrs.insert(key, value);
            }
        }
    }
    entry.name =
        name.ok_or_else(|| ParseError::invalid(pos(reader), "software without name attribute"))?;

    loop {
        match reader.read_event()? {
            Event::Start(e) => match e.name().as_ref() {
                b"description" => entry.description = Some(element_text(reader, &e)?),
                b"year" => entry.year = Some(element_text(reader, &e)?),
                b"publisher" => entry.manufacturer = Some(element_text(reader, &e)?),
                b"notes" => {
                    let text = element_text(reader, &e)?;
                    entry.attrs.insert("notes".into(), text);
                }
                b"part" => {
                    let part = parse_part(reader, &e, &mut entry)?;
                    entry.parts.push(part);
                }
                b"info" | b"sharedfeat" => {
                    named_value_attr(reader, &e, &mut entry)?;
                    skip_subtree(reader, &e)?;
                }
                _ => skip_subtree(reader, &e)?,
            },
            Event::Empty(e) => {
                if matches!(e.name().as_ref(), b"info" | b"sharedfeat") {
                    named_value_attr(reader, &e, &mut entry)?;
                }
            }
            Event::End(e) if e.name().as_ref() == b"software" => return Ok(entry),
            Event::Eof => return Err(ParseError::invalid(pos(reader), "unterminated software")),
            _ => {}
        }
    }
}

fn named_value_attr(
    reader: &Reader<&[u8]>,
    e: &BytesStart<'_>,
    entry: &mut Entry,
) -> Result<(), ParseError> {
    let prefix = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let (mut name, mut value) = (None, String::new());
    for attr in e.attributes() {
        let (key, val) = attr_pair(reader, &attr?)?;
        match key.as_str() {
            "name" => name = Some(val),
            "value" => value = val,
            _ => {}
        }
    }
    if let Some(name) = name {
        entry.attrs.insert(format!("{prefix}:{name}"), value);
    }
    Ok(())
}

fn parse_part(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
    entry: &mut Entry,
) -> Result<SoftwarePart, ParseError> {
    let mut part = SoftwarePart::default();
    for attr in start.attributes() {
        let (key, value) = attr_pair(reader, &attr?)?;
        match key.as_str() {
            "name" => part.name = value,
            "interface" => part.interface = value,
            _ => {
                part.features.insert(format!("attr:{key}"), value);
            }
        }
    }

    loop {
        match reader.read_event()? {
            Event::Start(e) => match e.name().as_ref() {
                b"dataarea" => {
                    let area = parse_dataarea(reader, &e, entry, false)?;
                    part.dataareas.push(area);
                }
                b"diskarea" => {
                    let area = parse_diskarea(reader, &e, entry, false)?;
                    part.diskareas.push(area);
                }
                b"feature" => {
                    part_feature(reader, &e, &mut part)?;
                    skip_subtree(reader, &e)?;
                }
                _ => skip_subtree(reader, &e)?,
            },
            Event::Empty(e) => match e.name().as_ref() {
                b"dataarea" => {
                    let area = parse_dataarea(reader, &e, entry, true)?;
                    part.dataareas.push(area);
                }
                b"diskarea" => {
                    let area = parse_diskarea(reader, &e, entry, true)?;
                    part.diskareas.push(area);
                }
                b"feature" => part_feature(reader, &e, &mut part)?,
                _ => {}
            },
            Event::End(e) if e.name().as_ref() == b"part" => return Ok(part),
            Event::Eof => return Err(ParseError::invalid(pos(reader), "unterminated part")),
            _ => {}
        }
    }
}

fn part_feature(
    reader: &Reader<&[u8]>,
    e: &BytesStart<'_>,
    part: &mut SoftwarePart,
) -> Result<(), ParseError> {
    let (mut name, mut value) = (None, String::new());
    for attr in e.attributes() {
        let (key, val) = attr_pair(reader, &attr?)?;
        match key.as_str() {
            "name" => name = Some(val),
            "value" => value = val,
            _ => {}
        }
    }
    if let Some(name) = name {
        part.features.insert(name, value);
    }
    Ok(())
}

fn parse_dataarea(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
    entry: &mut Entry,
    empty: bool,
) -> Result<DataArea, ParseError> {
    let mut area = DataArea::default();
    for attr in start.attributes() {
        let (key, value) = attr_pair(reader, &attr?)?;
        match key.as_str() {
            "name" => area.name = value,
            "size" => {
                area.size = Some(parse_size(&value).ok_or_else(|| {
                    ParseError::invalid(pos(reader), format!("bad dataarea size {value:?}"))
                })?);
            }
            "width" => area.width = Some(value),
            "endianness" => area.endianness = Some(value),
            _ => {}
        }
    }
    if empty {
        return Ok(area);
    }
    loop {
        match reader.read_event()? {
            Event::Start(e) if e.name().as_ref() == b"rom" => {
                area.claims.push(push_softlist_rom(reader, &e, entry)?);
                skip_subtree(reader, &e)?;
            }
            Event::Empty(e) if e.name().as_ref() == b"rom" => {
                area.claims.push(push_softlist_rom(reader, &e, entry)?);
            }
            Event::End(e) if e.name().as_ref() == b"dataarea" => return Ok(area),
            Event::Eof => return Err(ParseError::invalid(pos(reader), "unterminated dataarea")),
            _ => {}
        }
    }
}

fn parse_diskarea(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
    entry: &mut Entry,
    empty: bool,
) -> Result<DiskArea, ParseError> {
    let mut area = DiskArea::default();
    for attr in start.attributes() {
        let (key, value) = attr_pair(reader, &attr?)?;
        if key == "name" {
            area.name = value;
        }
    }
    if empty {
        return Ok(area);
    }
    loop {
        match reader.read_event()? {
            Event::Start(e) if e.name().as_ref() == b"disk" => {
                area.claims.push(push_softlist_disk(reader, &e, entry)?);
                skip_subtree(reader, &e)?;
            }
            Event::Empty(e) if e.name().as_ref() == b"disk" => {
                area.claims.push(push_softlist_disk(reader, &e, entry)?);
            }
            Event::End(e) if e.name().as_ref() == b"diskarea" => return Ok(area),
            Event::Eof => return Err(ParseError::invalid(pos(reader), "unterminated diskarea")),
            _ => {}
        }
    }
}

/// Parse one softlist rom row into the entry's flat claim list; returns its
/// index for the structural side.
fn push_softlist_rom(
    reader: &Reader<&[u8]>,
    e: &BytesStart<'_>,
    entry: &mut Entry,
) -> Result<usize, ParseError> {
    let mut claim = RomClaim::default();
    for attr in e.attributes() {
        let (key, value) = attr_pair(reader, &attr?)?;
        match key.as_str() {
            "name" => claim.name = value,
            "size" => {
                claim.size = Some(parse_size(&value).ok_or_else(|| {
                    ParseError::invalid(pos(reader), format!("bad rom size {value:?}"))
                })?);
            }
            "crc" => claim.crc32 = require_hash(reader, parse_crc32(&value), "crc", &value)?,
            "sha1" => claim.sha1 = require_hash(reader, parse_sha1(&value), "sha1", &value)?,
            "status" => {
                claim.status = ClaimStatus::parse(&value).ok_or_else(|| {
                    ParseError::invalid(pos(reader), format!("unknown status {value:?}"))
                })?;
            }
            // offset kept verbatim (module header: bare-hex ambiguity),
            // loadflag drives rebuild semantics post-MVP.
            _ => {
                claim.attrs.insert(key, value);
            }
        }
    }
    entry.claims.push(claim);
    Ok(entry.claims.len() - 1)
}

fn push_softlist_disk(
    reader: &Reader<&[u8]>,
    e: &BytesStart<'_>,
    entry: &mut Entry,
) -> Result<usize, ParseError> {
    let mut claim = RomClaim {
        kind: ClaimKind::Disk,
        ..RomClaim::default()
    };
    for attr in e.attributes() {
        let (key, value) = attr_pair(reader, &attr?)?;
        match key.as_str() {
            "name" => claim.name = value,
            "sha1" => claim.sha1 = require_hash(reader, parse_sha1(&value), "sha1", &value)?,
            "status" => {
                claim.status = ClaimStatus::parse(&value).ok_or_else(|| {
                    ParseError::invalid(pos(reader), format!("unknown status {value:?}"))
                })?;
            }
            _ => {
                claim.attrs.insert(key, value);
            }
        }
    }
    entry.claims.push(claim);
    Ok(entry.claims.len() - 1)
}
