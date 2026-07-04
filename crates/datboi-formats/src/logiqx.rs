//! Logiqx XML `datafile` parser — the lingua franca (No-Intro, Redump,
//! TOSEC, FBNeo, pleasuredome). Verified against the official DTD plus the
//! wild extensions 60-dats documents: No-Intro P/C `game@id`/`@cloneofid`,
//! `rom@sha256`/`@mia`/`@serial`, and `machine` as a synonym for `game`.
//!
//! Streaming discipline: quick-xml pull events only, one [`Entry`] built at
//! a time — no DOM. Unknown attributes are preserved into attrs maps
//! (gotcha 8); unknown *elements* are skipped whole (their information class
//! is unknown, and the CAS dat blob remains the canonical form).

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::model::{
    Attrs, ClaimKind, ClaimStatus, DatFile, DatHeader, Entry, ParseError, Release, RomClaim,
    is_yes, parse_crc32, parse_md5, parse_sha1, parse_sha256,
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
                b"datafile" => saw_root = true,
                b"header" => dat.header = parse_header(&mut reader)?,
                b"game" | b"machine" => {
                    dat.entries.push(parse_game(&mut reader, &e, false)?);
                }
                _ => skip_subtree(&mut reader, &e)?,
            },
            Event::Empty(e) => {
                if matches!(e.name().as_ref(), b"game" | b"machine") {
                    dat.entries.push(parse_game(&mut reader, &e, true)?);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_root {
        return Err(ParseError::invalid(pos(&reader), "no <datafile> root"));
    }
    Ok(dat)
}

fn parse_header(reader: &mut Reader<&[u8]>) -> Result<DatHeader, ParseError> {
    let mut h = DatHeader::default();
    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let name = e.name().as_ref().to_vec();
                match name.as_slice() {
                    b"name" => h.name = Some(element_text(reader, &e)?),
                    b"description" => h.description = Some(element_text(reader, &e)?),
                    b"version" => h.version = Some(element_text(reader, &e)?),
                    b"date" => h.date = Some(element_text(reader, &e)?),
                    b"author" => h.author = Some(element_text(reader, &e)?),
                    b"homepage" => h.homepage = Some(element_text(reader, &e)?),
                    b"url" => h.url = Some(element_text(reader, &e)?),
                    b"clrmamepro" => {
                        header_emitter_attrs(reader, &e, &mut h)?;
                        skip_subtree(reader, &e)?;
                    }
                    b"romcenter" => {
                        romcenter_attrs(reader, &e, &mut h.attrs)?;
                        skip_subtree(reader, &e)?;
                    }
                    _ => {
                        let key = String::from_utf8_lossy(&name).into_owned();
                        let text = element_text(reader, &e)?;
                        h.attrs.insert(key, text);
                    }
                }
            }
            Event::Empty(e) => match e.name().as_ref() {
                b"clrmamepro" => header_emitter_attrs(reader, &e, &mut h)?,
                b"romcenter" => romcenter_attrs(reader, &e, &mut h.attrs)?,
                _ => {}
            },
            Event::End(e) if e.name().as_ref() == b"header" => return Ok(h),
            Event::Eof => {
                return Err(ParseError::invalid(pos(reader), "unterminated <header>"));
            }
            _ => {}
        }
    }
}

fn header_emitter_attrs(
    reader: &Reader<&[u8]>,
    e: &BytesStart<'_>,
    h: &mut DatHeader,
) -> Result<(), ParseError> {
    for attr in e.attributes() {
        let (key, value) = attr_pair(reader, &attr?)?;
        match key.as_str() {
            "header" => h.detector = Some(value),
            "forcemerging" => h.force_merging = Some(value),
            "forcenodump" => h.force_nodump = Some(value),
            "forcepacking" => h.force_packing = Some(value),
            _ => {
                h.attrs.insert(format!("clrmamepro:{key}"), value);
            }
        }
    }
    Ok(())
}

fn romcenter_attrs(
    reader: &Reader<&[u8]>,
    e: &BytesStart<'_>,
    attrs: &mut Attrs,
) -> Result<(), ParseError> {
    for attr in e.attributes() {
        let (key, value) = attr_pair(reader, &attr?)?;
        attrs.insert(format!("romcenter:{key}"), value);
    }
    Ok(())
}

fn parse_game(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
    empty: bool,
) -> Result<Entry, ParseError> {
    let mut entry = Entry::default();
    let mut name = None;
    for attr in start.attributes() {
        let (key, value) = attr_pair(reader, &attr?)?;
        match key.as_str() {
            "name" => name = Some(value),
            "isbios" => entry.is_bios = is_yes(&value),
            "isdevice" => entry.is_device = is_yes(&value),
            "ismechanical" => entry.is_mechanical = is_yes(&value),
            "runnable" => entry.runnable = value != "no",
            "cloneof" => entry.cloneof = Some(value),
            "romof" => entry.romof = Some(value),
            "sampleof" => entry.sampleof = Some(value),
            "id" => entry.id = Some(value),
            "cloneofid" => entry.cloneof_id = Some(value),
            _ => {
                entry.attrs.insert(key, value);
            }
        }
    }
    entry.name =
        name.ok_or_else(|| ParseError::invalid(pos(reader), "game without name attribute"))?;
    if empty {
        return Ok(entry);
    }

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let name = e.name().as_ref().to_vec();
                match name.as_slice() {
                    b"description" => entry.description = Some(element_text(reader, &e)?),
                    b"year" => entry.year = Some(element_text(reader, &e)?),
                    b"manufacturer" => entry.manufacturer = Some(element_text(reader, &e)?),
                    b"comment" => {
                        let text = element_text(reader, &e)?;
                        entry
                            .attrs
                            .entry("comment".into())
                            .and_modify(|c| {
                                c.push('\n');
                                c.push_str(&text);
                            })
                            .or_insert(text);
                    }
                    b"rom" | b"disk" | b"sample" | b"release" | b"biosset" | b"archive" => {
                        game_child(reader, &e, &mut entry)?;
                        skip_subtree(reader, &e)?;
                    }
                    _ => skip_subtree(reader, &e)?,
                }
            }
            Event::Empty(e) => game_child(reader, &e, &mut entry)?,
            Event::End(e) if matches!(e.name().as_ref(), b"game" | b"machine") => {
                return Ok(entry);
            }
            Event::Eof => return Err(ParseError::invalid(pos(reader), "unterminated game")),
            _ => {}
        }
    }
}

/// Attr-only children shared by the Start and Empty event arms.
fn game_child(
    reader: &Reader<&[u8]>,
    e: &BytesStart<'_>,
    entry: &mut Entry,
) -> Result<(), ParseError> {
    match e.name().as_ref() {
        b"rom" => {
            let claim = parse_claim(reader, e, ClaimKind::Rom)?;
            entry.claims.push(claim);
        }
        b"disk" => {
            let claim = parse_claim(reader, e, ClaimKind::Disk)?;
            entry.claims.push(claim);
        }
        b"sample" => {
            let mut claim = RomClaim {
                kind: ClaimKind::Sample,
                ..RomClaim::default()
            };
            for attr in e.attributes() {
                let (key, value) = attr_pair(reader, &attr?)?;
                match key.as_str() {
                    "name" => claim.name = value,
                    _ => {
                        claim.attrs.insert(key, value);
                    }
                }
            }
            entry.claims.push(claim);
        }
        b"release" => {
            let mut release = Release::default();
            for attr in e.attributes() {
                let (key, value) = attr_pair(reader, &attr?)?;
                match key.as_str() {
                    "name" => release.name = value,
                    "region" => release.region = value,
                    "language" => release.language = Some(value),
                    "date" => release.date = Some(value),
                    "default" => release.is_default = is_yes(&value),
                    _ => {}
                }
            }
            entry.releases.push(release);
        }
        b"biosset" => {
            let (mut name, mut description, mut default) = (None, None, false);
            for attr in e.attributes() {
                let (key, value) = attr_pair(reader, &attr?)?;
                match key.as_str() {
                    "name" => name = Some(value),
                    "description" => description = Some(value),
                    "default" => default = is_yes(&value),
                    _ => {}
                }
            }
            if let Some(name) = name {
                let mut value = description.unwrap_or_default();
                if default {
                    value.push_str(" [default]");
                }
                entry.attrs.insert(format!("biosset:{name}"), value);
            }
        }
        b"archive" => {
            for attr in e.attributes() {
                let (key, value) = attr_pair(reader, &attr?)?;
                if key == "name" {
                    entry
                        .attrs
                        .insert(format!("archive:{value}"), String::new());
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn parse_claim(
    reader: &Reader<&[u8]>,
    e: &BytesStart<'_>,
    kind: ClaimKind,
) -> Result<RomClaim, ParseError> {
    let mut claim = RomClaim {
        kind,
        ..RomClaim::default()
    };
    for attr in e.attributes() {
        let (key, value) = attr_pair(reader, &attr?)?;
        match key.as_str() {
            "name" => claim.name = value,
            "size" => {
                claim.size = Some(value.parse().map_err(|_| {
                    ParseError::invalid(pos(reader), format!("bad rom size {value:?}"))
                })?);
            }
            "crc" => claim.crc32 = require_hash(reader, parse_crc32(&value), "crc", &value)?,
            "md5" => claim.md5 = require_hash(reader, parse_md5(&value), "md5", &value)?,
            "sha1" => claim.sha1 = require_hash(reader, parse_sha1(&value), "sha1", &value)?,
            "sha256" => {
                claim.sha256 = require_hash(reader, parse_sha256(&value), "sha256", &value)?;
            }
            "status" => {
                claim.status = ClaimStatus::parse(&value).ok_or_else(|| {
                    ParseError::invalid(pos(reader), format!("unknown status {value:?}"))
                })?;
            }
            "mia" => claim.mia = is_yes(&value),
            "optional" => claim.optional = is_yes(&value),
            "merge" => claim.merge_name = Some(value),
            _ => {
                claim.attrs.insert(key, value);
            }
        }
    }
    Ok(claim)
}

/// Empty hash attributes count as absent (seen in the wild); malformed
/// non-empty ones are hard errors — silently dropping a hash would weaken
/// claim unification (D2).
pub(crate) fn require_hash<T>(
    reader: &Reader<&[u8]>,
    parsed: Option<T>,
    what: &str,
    raw: &str,
) -> Result<Option<T>, ParseError> {
    if raw.is_empty() {
        return Ok(None);
    }
    match parsed {
        Some(v) => Ok(Some(v)),
        None => Err(ParseError::invalid(
            pos(reader),
            format!("malformed {what} value {raw:?}"),
        )),
    }
}
