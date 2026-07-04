//! dir2dat (D29): export a revision's claims as Logiqx XML.
//!
//! Determinism contract: byte-identical output for identical DB state —
//! entries sorted by name, claims in document (claim_id) order, fixed
//! formatting, no timestamps except those stored in the dat header.
//! Semantic (not byte) fidelity against the imported original is the bar
//! (60-dats losslessness; attribute order and whitespace may differ).
//!
//! Known asymmetry, documented: `optional` is a listxml notion with no
//! Logiqx DTD attribute; it is emitted as an extension attribute (like
//! No-Intro's `mia`) and round-trips through our own parser's attrs map
//! rather than the typed field.

use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use serde_json::Value;

use datboi_index::Db;

use crate::{CatalogError, audit};

const DOCTYPE: &str = r#" datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd""#;

/// Export `provider/system`'s current (or a specific, materialized)
/// revision as a Logiqx dat.
pub fn export_dat(
    db: &Db,
    provider: &str,
    system: &str,
    revision: Option<i64>,
) -> Result<Vec<u8>, CatalogError> {
    let conn = db.cache();
    let revision_id = match revision {
        Some(id) => id,
        None => audit::current_revision(db, provider, system)?,
    };
    let (header_json, materialized): (Option<String>, bool) = conn.query_row(
        "SELECT json(header), materialized FROM dat_revision WHERE revision_id = ?1",
        [revision_id],
        |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
    )?;
    if !materialized {
        return Err(CatalogError::Export(format!(
            "revision {revision_id} is header-only (demoted, D38); re-import it first"
        )));
    }
    let header: Value = header_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| CatalogError::Export(format!("stored header unparsable: {e}")))?
        .unwrap_or(Value::Null);

    let mut writer = Writer::new_with_indent(Vec::new(), b'\t', 1);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    writer.write_event(Event::DocType(BytesText::from_escaped(DOCTYPE)))?;
    writer.write_event(Event::Start(BytesStart::new("datafile")))?;

    write_header(&mut writer, &header)?;
    write_games(&mut writer, db, revision_id)?;

    writer.write_event(Event::End(BytesEnd::new("datafile")))?;
    Ok(writer.into_inner())
}

fn write_header(writer: &mut Writer<Vec<u8>>, header: &Value) -> Result<(), CatalogError> {
    writer.write_event(Event::Start(BytesStart::new("header")))?;
    for key in [
        "name",
        "description",
        "version",
        "date",
        "author",
        "homepage",
        "url",
    ] {
        if let Some(text) = header.get(key).and_then(Value::as_str) {
            write_text_element(writer, key, text)?;
        }
    }
    let force_keys = [
        ("force_merging", "forcemerging"),
        ("force_nodump", "forcenodump"),
        ("force_packing", "forcepacking"),
    ];
    let has_cmpro = force_keys
        .iter()
        .any(|(json_key, _)| header.get(json_key).and_then(Value::as_str).is_some())
        || header.get("detector").and_then(Value::as_str).is_some();
    if has_cmpro {
        let mut cmpro = BytesStart::new("clrmamepro");
        if let Some(detector) = header.get("detector").and_then(Value::as_str) {
            cmpro.push_attribute(("header", detector));
        }
        for (json_key, attr) in force_keys {
            if let Some(v) = header.get(json_key).and_then(Value::as_str) {
                cmpro.push_attribute((attr, v));
            }
        }
        writer.write_event(Event::Empty(cmpro))?;
    }
    writer.write_event(Event::End(BytesEnd::new("header")))?;
    Ok(())
}

fn write_games(
    writer: &mut Writer<Vec<u8>>,
    db: &Db,
    revision_id: i64,
) -> Result<(), CatalogError> {
    let conn = db.cache();
    let mut entries = conn.prepare(
        "SELECT entry_id, name, stable_key, description, year, manufacturer,
                is_bios, cloneof, romof, sampleof, json(attrs)
         FROM entry WHERE revision_id = ?1 ORDER BY name",
    )?;
    let rows: Vec<(i64, EntryRow)> = entries
        .query_map([revision_id], |row| {
            Ok((
                row.get(0)?,
                EntryRow {
                    name: row.get(1)?,
                    stable_key: row.get(2)?,
                    description: row.get(3)?,
                    year: row.get(4)?,
                    manufacturer: row.get(5)?,
                    is_bios: row.get::<_, i64>(6)? != 0,
                    cloneof: row.get(7)?,
                    romof: row.get(8)?,
                    sampleof: row.get(9)?,
                    attrs_json: row.get(10)?,
                },
            ))
        })?
        .collect::<Result<_, _>>()?;

    for (entry_id, entry) in rows {
        write_game(writer, conn, entry_id, &entry)?;
    }
    Ok(())
}

struct EntryRow {
    name: String,
    stable_key: Option<String>,
    description: Option<String>,
    year: Option<String>,
    manufacturer: Option<String>,
    is_bios: bool,
    cloneof: Option<String>,
    romof: Option<String>,
    sampleof: Option<String>,
    attrs_json: Option<String>,
}

fn write_game(
    writer: &mut Writer<Vec<u8>>,
    conn: &rusqlite::Connection,
    entry_id: i64,
    entry: &EntryRow,
) -> Result<(), CatalogError> {
    let attrs: Value = entry
        .attrs_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| CatalogError::Export(format!("entry attrs unparsable: {e}")))?
        .unwrap_or(Value::Null);

    let mut game = BytesStart::new("game");
    game.push_attribute(("name", entry.name.as_str()));
    if let Some(id) = &entry.stable_key {
        game.push_attribute(("id", id.as_str()));
    }
    if let Some(cid) = attrs.get("@cloneof_id").and_then(Value::as_str) {
        game.push_attribute(("cloneofid", cid));
    }
    if entry.is_bios {
        game.push_attribute(("isbios", "yes"));
    }
    for (key, value) in [
        ("cloneof", &entry.cloneof),
        ("romof", &entry.romof),
        ("sampleof", &entry.sampleof),
    ] {
        if let Some(v) = value {
            game.push_attribute((key, v.as_str()));
        }
    }
    writer.write_event(Event::Start(game))?;

    if let Some(text) = &entry.description {
        write_text_element(writer, "description", text)?;
    }
    if let Some(text) = &entry.year {
        write_text_element(writer, "year", text)?;
    }
    if let Some(text) = &entry.manufacturer {
        write_text_element(writer, "manufacturer", text)?;
    }

    // (name, region, language, date, default) — one <release/> row.
    type ReleaseRow = (String, String, Option<String>, Option<String>, bool);
    let mut releases = conn.prepare_cached(
        "SELECT name, region, language, rel_date, is_default FROM release
         WHERE entry_id = ?1 ORDER BY rowid",
    )?;
    let release_rows: Vec<ReleaseRow> = releases
        .query_map([entry_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get::<_, i64>(4)? != 0,
            ))
        })?
        .collect::<Result<_, _>>()?;
    for (name, region, language, date, is_default) in &release_rows {
        let mut rel = BytesStart::new("release");
        rel.push_attribute(("name", name.as_str()));
        rel.push_attribute(("region", region.as_str()));
        if let Some(l) = language {
            rel.push_attribute(("language", l.as_str()));
        }
        if let Some(d) = date {
            rel.push_attribute(("date", d.as_str()));
        }
        if *is_default {
            rel.push_attribute(("default", "yes"));
        }
        writer.write_event(Event::Empty(rel))?;
    }

    let mut claims = conn.prepare_cached(
        "SELECT kind, name, size, crc32, md5, sha1, sha256, status, mia,
                optional, merge_name
         FROM rom_claim WHERE entry_id = ?1 ORDER BY claim_id",
    )?;
    let claim_rows: Vec<ClaimRow> = claims
        .query_map([entry_id], |row| {
            Ok(ClaimRow {
                kind: row.get(0)?,
                name: row.get(1)?,
                size: row.get(2)?,
                crc32: row.get(3)?,
                md5: row.get(4)?,
                sha1: row.get(5)?,
                sha256: row.get(6)?,
                status: row.get(7)?,
                mia: row.get::<_, i64>(8)? != 0,
                optional: row.get::<_, i64>(9)? != 0,
                merge_name: row.get(10)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    for claim in &claim_rows {
        write_claim(writer, claim)?;
    }

    writer.write_event(Event::End(BytesEnd::new("game")))?;
    Ok(())
}

struct ClaimRow {
    kind: i64,
    name: String,
    size: Option<i64>,
    crc32: Option<Vec<u8>>,
    md5: Option<Vec<u8>>,
    sha1: Option<Vec<u8>>,
    sha256: Option<Vec<u8>>,
    status: i64,
    mia: bool,
    optional: bool,
    merge_name: Option<String>,
}

fn write_claim(writer: &mut Writer<Vec<u8>>, claim: &ClaimRow) -> Result<(), CatalogError> {
    let element = match claim.kind {
        0 => "rom",
        1 => "disk",
        2 => "sample",
        other => return Err(CatalogError::Export(format!("unknown claim kind {other}"))),
    };
    let mut el = BytesStart::new(element);
    el.push_attribute(("name", claim.name.as_str()));
    if element == "rom"
        && let Some(size) = claim.size
    {
        el.push_attribute(("size", size.to_string().as_str()));
    }
    for (attr, digest) in [
        ("crc", &claim.crc32),
        ("md5", &claim.md5),
        ("sha1", &claim.sha1),
        ("sha256", &claim.sha256),
    ] {
        if element == "sample" {
            break; // samples carry no hashes
        }
        if let Some(digest) = digest {
            el.push_attribute((attr, hex(digest).as_str()));
        }
    }
    if let Some(merge) = &claim.merge_name {
        el.push_attribute(("merge", merge.as_str()));
    }
    let status = match claim.status {
        0 => None,
        1 => Some("baddump"),
        2 => Some("nodump"),
        3 => Some("verified"),
        other => return Err(CatalogError::Export(format!("unknown status {other}"))),
    };
    if let Some(status) = status {
        el.push_attribute(("status", status));
    }
    if claim.mia {
        el.push_attribute(("mia", "yes"));
    }
    if claim.optional {
        el.push_attribute(("optional", "yes"));
    }
    writer.write_event(Event::Empty(el))?;
    Ok(())
}

fn write_text_element(
    writer: &mut Writer<Vec<u8>>,
    name: &str,
    text: &str,
) -> Result<(), CatalogError> {
    writer.write_event(Event::Start(BytesStart::new(name)))?;
    writer.write_event(Event::Text(BytesText::new(text)))?;
    writer.write_event(Event::End(BytesEnd::new(name)))?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("string write");
    }
    s
}
