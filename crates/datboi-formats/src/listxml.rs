//! MAME `-listxml` parser. Machines share the Logiqx claim shape (crc+sha1,
//! no md5 — dats gotcha 2) with extra audit-relevant structure:
//! `device_ref` (captured for post-MVP closure queries, D31), bios sets,
//! `disk` internal sha1s, driver/feature status. Emulation-only elements
//! (chip/display/input/dipswitch/…) are skipped without buffering.

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::logiqx::parse_claim;
use crate::model::{ClaimKind, DatFile, Entry, ParseError, RomClaim, is_yes};
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
                b"mame" => {
                    saw_root = true;
                    dat.header.name = Some("MAME".into());
                    for attr in e.attributes() {
                        let (key, value) = attr_pair(&reader, &attr?)?;
                        if key == "build" {
                            dat.header.version = Some(value);
                        } else {
                            dat.header.attrs.insert(key, value);
                        }
                    }
                }
                b"machine" | b"game" => {
                    dat.entries.push(parse_machine(&mut reader, &e)?);
                }
                _ => skip_subtree(&mut reader, &e)?,
            },
            Event::Empty(e) => {
                if matches!(e.name().as_ref(), b"machine" | b"game") {
                    dat.entries.push(machine_from_attrs(&reader, &e)?);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_root {
        return Err(ParseError::invalid(pos(&reader), "no <mame> root"));
    }
    Ok(dat)
}

fn machine_from_attrs(reader: &Reader<&[u8]>, start: &BytesStart<'_>) -> Result<Entry, ParseError> {
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
            _ => {
                entry.attrs.insert(key, value);
            }
        }
    }
    entry.name =
        name.ok_or_else(|| ParseError::invalid(pos(reader), "machine without name attribute"))?;
    Ok(entry)
}

fn parse_machine(reader: &mut Reader<&[u8]>, start: &BytesStart<'_>) -> Result<Entry, ParseError> {
    let mut entry = machine_from_attrs(reader, start)?;

    loop {
        match reader.read_event()? {
            Event::Start(e) => match e.name().as_ref() {
                b"description" => entry.description = Some(element_text(reader, &e)?),
                b"year" => entry.year = Some(element_text(reader, &e)?),
                b"manufacturer" => entry.manufacturer = Some(element_text(reader, &e)?),
                b"rom" | b"disk" | b"sample" | b"device_ref" | b"biosset" | b"feature"
                | b"driver" | b"softwarelist" => {
                    machine_child(reader, &e, &mut entry)?;
                    skip_subtree(reader, &e)?;
                }
                _ => skip_subtree(reader, &e)?,
            },
            Event::Empty(e) => machine_child(reader, &e, &mut entry)?,
            Event::End(e) if matches!(e.name().as_ref(), b"machine" | b"game") => {
                return Ok(entry);
            }
            Event::Eof => return Err(ParseError::invalid(pos(reader), "unterminated machine")),
            _ => {}
        }
    }
}

fn machine_child(
    reader: &Reader<&[u8]>,
    e: &BytesStart<'_>,
    entry: &mut Entry,
) -> Result<(), ParseError> {
    match e.name().as_ref() {
        b"rom" => entry.claims.push(parse_claim(reader, e, ClaimKind::Rom)?),
        b"disk" => entry.claims.push(parse_claim(reader, e, ClaimKind::Disk)?),
        b"sample" => {
            let mut claim = RomClaim {
                kind: ClaimKind::Sample,
                ..RomClaim::default()
            };
            for attr in e.attributes() {
                let (key, value) = attr_pair(reader, &attr?)?;
                if key == "name" {
                    claim.name = value;
                }
            }
            entry.claims.push(claim);
        }
        b"device_ref" => {
            for attr in e.attributes() {
                let (key, value) = attr_pair(reader, &attr?)?;
                if key == "name" {
                    entry.device_refs.push(value);
                }
            }
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
        b"feature" => {
            let (mut ftype, mut status, mut overall) = (None, None, None);
            for attr in e.attributes() {
                let (key, value) = attr_pair(reader, &attr?)?;
                match key.as_str() {
                    "type" => ftype = Some(value),
                    "status" => status = Some(value),
                    "overall" => overall = Some(value),
                    _ => {}
                }
            }
            if let Some(ftype) = ftype {
                let mut value = status.unwrap_or_default();
                if let Some(overall) = overall {
                    value.push_str(";overall=");
                    value.push_str(&overall);
                }
                entry.attrs.insert(format!("feature:{ftype}"), value);
            }
        }
        b"driver" => {
            for attr in e.attributes() {
                let (key, value) = attr_pair(reader, &attr?)?;
                entry.attrs.insert(format!("driver:{key}"), value);
            }
        }
        b"softwarelist" => {
            let (mut name, mut rest) = (None, Vec::new());
            for attr in e.attributes() {
                let (key, value) = attr_pair(reader, &attr?)?;
                match key.as_str() {
                    "name" => name = Some(value),
                    _ => rest.push(format!("{key}={value}")),
                }
            }
            if let Some(name) = name {
                entry
                    .attrs
                    .insert(format!("softwarelist:{name}"), rest.join(";"));
            }
        }
        _ => {}
    }
    Ok(())
}
