//! View definitions + evaluation (M4, 80-views.md three layers):
//! a *definition* is mutable policy (state.db config KV — it may say
//! "current"); *evaluation* resolves it against the current dat revision
//! and grounded holdings into an immutable `datboi/viewsnap/1` manifest;
//! the view's TAG (`view/<name>`) points at the latest snapshot — the
//! D33 atomic flip and the D27 GC root are the same tag move.
//!
//! v1 scope (ratified surface only): query = one dat source's current
//! revision ∩ have(verified); selection = all required claims;
//! transform chain = none; layout = template over `{entry}` / `{name}`.
//! 1G1R, profiles, and transform chains land on this same shape.

use datboi_core::cbor::{self, Value};
use datboi_core::hash::Blake3;
use datboi_core::viewsnap::{ViewRow, ViewSnapshot, ViewSource, path_is_canonical};
use datboi_index::Db;
use datboi_store_fs::{Namespace as StoreNs, Store};
use rusqlite::params;

use crate::{CatalogError, audit::current_revision, rollup::refresh_rollups, unify::relink_all};

// Definition CBOR: {1: provider, 2: system, 3: template}.
const DEFKEY_PROVIDER: u64 = 1;
const DEFKEY_SYSTEM: u64 = 2;
const DEFKEY_TEMPLATE: u64 = 3;

/// A named view definition (the mutable policy layer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDef {
    pub name: String,
    pub provider: String,
    pub system: String,
    /// Layout template. Placeholders: `{entry}` (game name), `{name}`
    /// (rom claim name). Path separators inside expanded values are
    /// sanitized to `_`.
    pub template: String,
}

fn def_key(name: &str) -> String {
    format!("view:{name}")
}

/// Store or replace a definition.
///
/// # Errors
/// Index I/O.
pub fn define_view(db: &Db, def: &ViewDef) -> Result<(), CatalogError> {
    let bytes = cbor::encode(&Value::Map(vec![
        (DEFKEY_PROVIDER, Value::Text(def.provider.clone())),
        (DEFKEY_SYSTEM, Value::Text(def.system.clone())),
        (DEFKEY_TEMPLATE, Value::Text(def.template.clone())),
    ]))
    .expect("static keys");
    db.config_set(&def_key(&def.name), &bytes)?;
    Ok(())
}

/// Load one definition.
///
/// # Errors
/// Index I/O; `None` if undefined.
pub fn get_view(db: &Db, name: &str) -> Result<Option<ViewDef>, CatalogError> {
    let Some(bytes) = db.config_get(&def_key(name))? else {
        return Ok(None);
    };
    let Value::Map(pairs) = cbor::decode(&bytes).map_err(|_| CatalogError::Corrupt("view def"))?
    else {
        return Err(CatalogError::Corrupt("view def"));
    };
    let (mut provider, mut system, mut template) = (None, None, None);
    for (key, value) in pairs {
        match (key, value) {
            (DEFKEY_PROVIDER, Value::Text(v)) => provider = Some(v),
            (DEFKEY_SYSTEM, Value::Text(v)) => system = Some(v),
            (DEFKEY_TEMPLATE, Value::Text(v)) => template = Some(v),
            _ => return Err(CatalogError::Corrupt("view def")),
        }
    }
    Ok(Some(ViewDef {
        name: name.to_owned(),
        provider: provider.ok_or(CatalogError::Corrupt("view def"))?,
        system: system.ok_or(CatalogError::Corrupt("view def"))?,
        template: template.ok_or(CatalogError::Corrupt("view def"))?,
    }))
}

/// All defined view names.
///
/// # Errors
/// Index I/O.
pub fn list_views(db: &Db) -> Result<Vec<String>, CatalogError> {
    let mut names = Vec::new();
    for (key, _) in db.config_list_prefix("view:")? {
        names.push(key["view:".len()..].to_owned());
    }
    Ok(names)
}

/// Report of one evaluation.
#[derive(Debug)]
pub struct EvalReport {
    pub snapshot: Blake3,
    pub rows: usize,
    /// Claims that matched no grounded blob (missing/unverified).
    pub missing: usize,
    /// Rows renamed to resolve path collisions.
    pub disambiguated: usize,
}

/// Evaluate a definition into an immutable snapshot, publish it (meta
/// namespace), and flip the view tag (D33).
///
/// # Errors
/// Unknown source, store/index I/O.
pub fn evaluate_view(
    db: &mut Db,
    store: &Store,
    def: &ViewDef,
    now: i64,
) -> Result<EvalReport, CatalogError> {
    // Ingest runs may post-date the dat import (import links only what it
    // touched): re-link identities, then recompute rollups, so the
    // snapshot sees current holdings.
    relink_all(db)?;
    refresh_rollups(db, now)?;
    let revision_id = current_revision(db, &def.provider, &def.system)?;
    let (dat_blob, missing_total): (Blake3, u64) = {
        let conn = db.cache();
        let dat_blob: [u8; 32] = conn.query_row(
            "SELECT b.hash FROM dat_revision r JOIN blob b ON b.blob_id = r.blob_id
             WHERE r.revision_id = ?1",
            params![revision_id],
            |row| row.get(0),
        )?;
        let missing: i64 = conn.query_row(
            "SELECT COALESCE(SUM(ea.missing), 0) FROM entry e
             JOIN entry_audit ea ON ea.entry_id = e.entry_id
             WHERE e.revision_id = ?1",
            params![revision_id],
            |row| row.get(0),
        )?;
        (Blake3(dat_blob), u64::try_from(missing).unwrap_or(0))
    };

    // Every required claim of the current revision with a have(verified)
    // identity, resolved to a deterministic blob (smallest hash wins when
    // several blobs share the identity).
    struct Picked {
        entry: String,
        claim: String,
        hash: Blake3,
        size: u64,
    }
    let mut picked: Vec<Picked> = Vec::new();
    {
        let conn = db.cache();
        let mut stmt = conn.prepare(
            "SELECT e.name, rc.name, MIN(b.hash), MAX(b.size)
             FROM entry e
             JOIN rom_claim rc ON rc.entry_id = e.entry_id
             JOIN identity_status s ON s.identity_id = rc.identity_id AND s.state = 4
             JOIN identity_blob ib ON ib.identity_id = rc.identity_id AND ib.basis >= 1
             JOIN blob b ON b.blob_id = ib.blob_id
             WHERE e.revision_id = ?1 AND rc.status != 2 AND NOT rc.optional
             GROUP BY rc.claim_id
             ORDER BY e.name, rc.name",
        )?;
        let rows = stmt.query_map(params![revision_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, [u8; 32]>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;
        for row in rows {
            let (entry, claim, hash, size) = row?;
            picked.push(Picked {
                entry,
                claim,
                hash: Blake3(hash),
                size: u64::try_from(size.unwrap_or(0)).unwrap_or(0),
            });
        }
    }

    // Layout + collision handling. Sanitized expansion first; identical
    // paths get a deterministic ` (xxxxxxxx)` hash suffix.
    let mut rows: Vec<ViewRow> = Vec::new();
    let mut disambiguated = 0usize;
    let mut seen = std::collections::HashSet::new();
    for p in &picked {
        let mut path = def
            .template
            .replace("{entry}", &sanitize_component(&p.entry))
            .replace("{name}", &sanitize_name(&p.claim));
        if !path_is_canonical(&path) {
            path = sanitize_name(&path);
        }
        if !seen.insert(path.clone()) {
            let tag = &p.hash.to_hex()[..8];
            path = match path.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({tag}).{ext}"),
                _ => format!("{path} ({tag})"),
            };
            if !seen.insert(path.clone()) {
                continue; // same blob mapped twice: one row is enough
            }
            disambiguated += 1;
        }
        let seek = seek_class_of(db, &p.hash)?;
        rows.push(ViewRow {
            path,
            hash: p.hash,
            size: p.size,
            seek,
        });
    }

    let snap = ViewSnapshot {
        created_at: u64::try_from(now).unwrap_or(0),
        view_name: def.name.clone(),
        sources: vec![ViewSource {
            provider: def.provider.clone(),
            system: def.system.clone(),
            dat_blob,
            revision: u64::try_from(revision_id).unwrap_or(0),
        }],
        rows,
    };
    let encoded = snap.encode().map_err(|_| CatalogError::Corrupt("viewsnap"))?;
    let hash = Blake3::compute(&encoded);
    store.put(StoreNs::Meta, hash, encoded.as_slice())?;
    db.upsert_blob(
        &hash,
        Some(encoded.len() as u64),
        datboi_index::Namespace::Meta,
        datboi_index::Residency::Resident,
    )?;
    // The atomic flip + GC root (D33/D27): one tag move.
    db.set_tag(&format!("view/{}", def.name), &hash, now)?;
    Ok(EvalReport {
        snapshot: hash,
        rows: snap.rows.len(),
        missing: usize::try_from(missing_total).unwrap_or(usize::MAX),
        disambiguated,
    })
}

/// D27 class for a blob at snapshot time: resident literals read
/// affinely; recipe-covered blobs inherit their best route's class.
fn seek_class_of(db: &Db, hash: &Blake3) -> Result<u8, CatalogError> {
    let Some(row) = db.blob_by_hash(hash)? else {
        return Ok(2); // unknown: treat as opaque, never guess seekable
    };
    if row.residency == datboi_index::Residency::Resident {
        return Ok(0);
    }
    let mut best = 2u8;
    for recipe in db.recipes_for_output(row.blob_id)? {
        if recipe.verify == datboi_index::VerifyState::Failed {
            continue;
        }
        best = best.min(match recipe.seek_class {
            datboi_index::SeekClass::Affine => 0,
            datboi_index::SeekClass::ManifestSeekable => 1,
            datboi_index::SeekClass::Opaque => 2,
        });
    }
    Ok(best)
}

/// One path component: no separators, no NUL, never `.`/`..`/empty.
fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == '\0' { '_' } else { c })
        .collect();
    match cleaned.as_str() {
        "" | "." | ".." => "_".into(),
        _ => cleaned,
    }
}

/// A claim name: forward slashes allowed (dats use nested rom names),
/// each component sanitized.
fn sanitize_name(s: &str) -> String {
    let parts: Vec<String> = s
        .replace('\\', "/")
        .split('/')
        .filter(|c| !c.is_empty())
        .map(sanitize_component)
        .collect();
    if parts.is_empty() {
        "_".into()
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizers_produce_canonical_components() {
        assert_eq!(sanitize_component("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(sanitize_name("sub\\dir/rom.bin"), "sub/dir/rom.bin");
        assert_eq!(sanitize_name("//../x"), "_/x");
        assert!(path_is_canonical(&sanitize_name("weird\\..\\path")));
    }
}
