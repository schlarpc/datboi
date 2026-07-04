//! Dat import (docs/60-dats.md): dat blob into CAS first, then rows are a
//! deterministic function of that blob (D15). Revision retention per D38:
//! current + previous stay materialized; older revisions demote to
//! header-only (rows deleted; re-importable on demand from the CAS blob).

use datboi_core::hash::Blake3;
use datboi_formats::model::{ClaimKind, ClaimStatus, DatFile, DatHeader, Entry};
use datboi_formats::{DatFormat, detect, parse};
use datboi_index::dats::{NewClaim, NewEntry, NewRelease};
use datboi_index::{Db, Namespace, Residency};
use datboi_store_fs::Store;
use serde_json::{Map, Value, json};

use crate::{CatalogError, rollup, unify};

/// dat_revision.format codes (65-schema.md §2). Frozen with SCHEMA_VERSION.
#[must_use]
pub fn format_code(format: DatFormat) -> i64 {
    match format {
        DatFormat::Logiqx => 0,
        DatFormat::ClrMamePro => 1,
        DatFormat::RomCenter => 2,
        DatFormat::MameListXml => 3,
        DatFormat::MameSoftwareList => 4,
    }
}

#[derive(Debug, Default)]
pub struct ImportOptions<'a> {
    /// Dat source identity; defaults derive from the dat header
    /// (provider ← author, system ← name — override for real providers).
    pub provider: Option<&'a str>,
    pub system: Option<&'a str>,
    /// Unix seconds; the single wall-clock value an import may record.
    pub imported_at: i64,
}

#[derive(Debug)]
pub struct ImportReport {
    pub source_id: i64,
    pub revision_id: i64,
    pub dat_blob: Blake3,
    pub entries: u64,
    pub claims: u64,
    /// Revisions demoted to header-only by this import (D38).
    pub demoted_revisions: Vec<i64>,
}

/// Import one dat file: CAS blob, revision rows, unification, rollups.
pub fn import_dat(
    store: &Store,
    db: &mut Db,
    bytes: &[u8],
    opts: &ImportOptions<'_>,
) -> Result<ImportReport, CatalogError> {
    // Dat files are opaque payloads, not datboi structured objects: data/.
    let (hash, aliases, _outcome) = store.put_new(datboi_store_fs::Namespace::Data, bytes)?;
    let blob_id = db.upsert_blob(
        &hash,
        Some(aliases.size),
        Namespace::Data,
        Residency::Resident,
    )?;
    db.insert_aliases(blob_id, &aliases)?;
    db.set_verified(blob_id, opts.imported_at)?;

    let format = detect(bytes).ok_or(datboi_formats::model::ParseError::UnknownFormat)?;
    let dat = parse(bytes)?;

    let provider = opts
        .provider
        .or(dat.header.author.as_deref())
        .unwrap_or("unknown");
    let system = opts
        .system
        .or(dat.header.name.as_deref())
        .unwrap_or("unknown");

    let source_id = db.upsert_dat_source(provider, system)?;
    let header_json = header_to_json(&dat.header);
    let revision_id = db.insert_dat_revision(
        source_id,
        blob_id,
        format_code(format),
        dat.header.version.as_deref(),
        dat.header.date.as_deref(),
        Some(&header_json),
        None, // detector registration is ingest-side; linked later
        opts.imported_at,
    )?;

    let rendered = render_attrs(&dat);
    let (new_entries, claims) = to_new_entries(&dat, &rendered);
    let entries = db.insert_entries(revision_id, &new_entries)?;
    db.set_current_revision(source_id, revision_id)?;

    let touched = unify::unify_revision(db, revision_id)?;
    unify::link_identities_to_blobs(db, &touched)?;
    let demoted_revisions = demote_old_revisions(db, source_id)?;
    rollup::refresh_rollups(db, opts.imported_at)?;

    Ok(ImportReport {
        source_id,
        revision_id,
        dat_blob: hash,
        entries,
        claims,
        demoted_revisions,
    })
}

/// D38: keep the two newest revisions of a source materialized; strip the
/// rows of anything older. Plain DELETE — the CAS blob re-imports on
/// demand (deletion as archival, D15).
fn demote_old_revisions(db: &Db, source_id: i64) -> Result<Vec<i64>, CatalogError> {
    let conn = db.cache();
    let mut stmt = conn.prepare_cached(
        "SELECT revision_id FROM dat_revision
         WHERE source_id = ?1 AND materialized = 1
         ORDER BY revision_id DESC",
    )?;
    let revisions: Vec<i64> = stmt
        .query_map([source_id], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut demoted = Vec::new();
    for &revision_id in revisions.iter().skip(2) {
        let tx = conn.unchecked_transaction()?;
        for sql in [
            "DELETE FROM entry_audit WHERE entry_id IN
               (SELECT entry_id FROM entry WHERE revision_id = ?1)",
            "DELETE FROM annotation WHERE entry_id IN
               (SELECT entry_id FROM entry WHERE revision_id = ?1)",
            "DELETE FROM rom_claim WHERE entry_id IN
               (SELECT entry_id FROM entry WHERE revision_id = ?1)",
            "DELETE FROM release WHERE entry_id IN
               (SELECT entry_id FROM entry WHERE revision_id = ?1)",
            "DELETE FROM entry WHERE revision_id = ?1",
            "UPDATE dat_revision SET materialized = 0 WHERE revision_id = ?1",
        ] {
            tx.execute(sql, [revision_id])?;
        }
        tx.commit()?;
        demoted.push(revision_id);
    }
    Ok(demoted)
}

/// Pre-rendered JSON attrs (one per entry, one per claim). Rendered ahead
/// of `NewEntry` construction because the index types borrow `&str`.
struct RenderedAttrs {
    entries: Vec<(Option<String>, Vec<Option<String>>)>,
}

fn to_new_entries<'a>(dat: &'a DatFile, rendered: &'a RenderedAttrs) -> (Vec<NewEntry<'a>>, u64) {
    let mut claims_total = 0u64;
    let entries = dat
        .entries
        .iter()
        .zip(&rendered.entries)
        .map(|(entry, (entry_attrs, claim_attrs))| {
            claims_total += entry.claims.len() as u64;
            NewEntry {
                name: &entry.name,
                stable_key: entry.id.as_deref(),
                description: entry.description.as_deref(),
                year: entry.year.as_deref(),
                manufacturer: entry.manufacturer.as_deref(),
                is_bios: entry.is_bios,
                is_device: entry.is_device,
                is_mechanical: entry.is_mechanical,
                runnable: entry.runnable,
                cloneof: entry.cloneof.as_deref(),
                romof: entry.romof.as_deref(),
                sampleof: entry.sampleof.as_deref(),
                attrs: entry_attrs.as_deref(),
                releases: entry
                    .releases
                    .iter()
                    .map(|r| NewRelease {
                        name: &r.name,
                        region: &r.region,
                        language: r.language.as_deref(),
                        date: r.date.as_deref(),
                        is_default: r.is_default,
                    })
                    .collect(),
                claims: entry
                    .claims
                    .iter()
                    .zip(claim_attrs)
                    .map(|(c, attrs)| NewClaim {
                        kind: match c.kind {
                            ClaimKind::Rom => datboi_index::ClaimKind::Rom,
                            ClaimKind::Disk => datboi_index::ClaimKind::Disk,
                            ClaimKind::Sample => datboi_index::ClaimKind::Sample,
                        },
                        name: &c.name,
                        size: c.size,
                        crc32: c.crc32,
                        md5: c.md5,
                        sha1: c.sha1,
                        sha256: c.sha256,
                        status: match c.status {
                            ClaimStatus::Good => datboi_index::ClaimStatus::Good,
                            ClaimStatus::BadDump => datboi_index::ClaimStatus::BadDump,
                            ClaimStatus::NoDump => datboi_index::ClaimStatus::NoDump,
                            ClaimStatus::Verified => datboi_index::ClaimStatus::Verified,
                        },
                        mia: c.mia,
                        optional: c.optional,
                        merge_name: c.merge_name.as_deref(),
                        attrs: attrs.as_deref(),
                    })
                    .collect(),
            }
        })
        .collect();
    (entries, claims_total)
}

fn render_attrs(dat: &DatFile) -> RenderedAttrs {
    RenderedAttrs {
        entries: dat
            .entries
            .iter()
            .map(|entry| {
                let claim_attrs = entry
                    .claims
                    .iter()
                    .map(|c| (!c.attrs.is_empty()).then(|| json!(c.attrs).to_string()))
                    .collect();
                (render_entry_attrs(entry), claim_attrs)
            })
            .collect(),
    }
}

/// Entry attrs JSON: the parser's flat attrs map, plus reserved `@`-keys
/// for structured extras (parser attrs use `elem:name` namespacing, never
/// a bare `@` prefix, so collisions are impossible):
/// `@cloneof_id` (No-Intro numeric parent), `@device_refs`, `@parts`
/// (lossless softlist structure, D13 — claim references are indices into
/// the entry's rom_claim rows in insertion order).
fn render_entry_attrs(entry: &Entry) -> Option<String> {
    let mut map = Map::new();
    for (k, v) in &entry.attrs {
        map.insert(k.clone(), Value::String(v.clone()));
    }
    if let Some(cloneof_id) = &entry.cloneof_id {
        map.insert("@cloneof_id".to_owned(), Value::String(cloneof_id.clone()));
    }
    if !entry.device_refs.is_empty() {
        map.insert("@device_refs".to_owned(), json!(entry.device_refs));
    }
    if !entry.parts.is_empty() {
        let parts: Vec<Value> = entry
            .parts
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "interface": p.interface,
                    "features": p.features,
                    "dataareas": p.dataareas.iter().map(|a| json!({
                        "name": a.name,
                        "size": a.size,
                        "width": a.width,
                        "endianness": a.endianness,
                        "claims": a.claims,
                    })).collect::<Vec<_>>(),
                    "diskareas": p.diskareas.iter().map(|a| json!({
                        "name": a.name,
                        "claims": a.claims,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        map.insert("@parts".to_owned(), Value::Array(parts));
    }
    (!map.is_empty()).then(|| Value::Object(map).to_string())
}

fn header_to_json(header: &DatHeader) -> String {
    let mut map = Map::new();
    let mut put = |k: &str, v: &Option<String>| {
        if let Some(v) = v {
            map.insert(k.to_owned(), Value::String(v.clone()));
        }
    };
    put("name", &header.name);
    put("description", &header.description);
    put("version", &header.version);
    put("date", &header.date);
    put("author", &header.author);
    put("homepage", &header.homepage);
    put("url", &header.url);
    put("force_merging", &header.force_merging);
    put("force_nodump", &header.force_nodump);
    put("force_packing", &header.force_packing);
    put("detector", &header.detector);
    if !header.attrs.is_empty() {
        map.insert("attrs".to_owned(), json!(header.attrs));
    }
    Value::Object(map).to_string()
}
