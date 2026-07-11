//! View definitions + evaluation (M4, 80-views.md three layers):
//! a *definition* is mutable policy (state.db config KV — it may say
//! "current"); *evaluation* resolves it against the current dat revision
//! and grounded holdings into an immutable `datboi/viewsnap/1` manifest;
//! the view's TAG (`view/<name>`) points at the latest snapshot — the
//! D33 atomic flip and the D27 GC root are the same tag move.
//!
//! v1 scope: query = one dat source's current revision ∩ have(verified);
//! selection = all required claims, or 1G1R over clone families
//! ([`crate::selection`]); layout = template over `{entry}` / `{name}`,
//! optionally constrained by a device profile ([`crate::profiles`]).
//! Transform chains land on this same shape.

use std::collections::{HashMap, HashSet};

use datboi_core::cbor::{self, Value};
use datboi_core::hash::Blake3;
use datboi_core::viewsnap::{ViewRow, ViewSnapshot, ViewSource, path_is_canonical};
use datboi_index::Db;
use datboi_store_fs::{Namespace as StoreNs, Store};
use rusqlite::params;

use crate::selection::{Candidate, SelectionPolicy, select_1g1r};
use crate::{CatalogError, audit::current_revision, rollup::refresh_rollups, unify::relink_all};

// Definition CBOR: {1: provider, 2: system, 3: template, 4: selection
// mode (0 all / 1 one-per-family held-first / 2 one-per-family strict,
// D57), 5: regions, 6: langs, 7: profile, 8: image mode (1 = FAT32),
// 9: image cluster size, 10: image partition (0/1), 11: image label}.
// Keys 4–7 are additive: v1 definitions decode as mode 0, no profile.
// Keys 8–11 are additive the same way (D62): absent = no image mode.
// Mode 2 is additive within key 4's existing vocabulary.
const DEFKEY_PROVIDER: u64 = 1;
const DEFKEY_SYSTEM: u64 = 2;
const DEFKEY_TEMPLATE: u64 = 3;
const DEFKEY_SELECTION: u64 = 4;
const DEFKEY_REGIONS: u64 = 5;
const DEFKEY_LANGS: u64 = 6;
const DEFKEY_PROFILE: u64 = 7;
const DEFKEY_IMAGE: u64 = 8;
const DEFKEY_IMAGE_CLUSTER: u64 = 9;
const DEFKEY_IMAGE_PARTITION: u64 = 10;
const DEFKEY_IMAGE_LABEL: u64 = 11;

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
    /// `Some` = 1G1R over clone families with these priorities.
    pub selection: Option<SelectionPolicy>,
    /// Built-in constraint profile name ([`crate::profiles`]).
    pub profile: Option<String>,
    /// `Some` = the view also reifies as a FAT32 image (D62).
    pub image: Option<crate::image::ImageParams>,
}

fn def_key(name: &str) -> String {
    format!("view:{name}")
}

/// Store or replace a definition.
///
/// # Errors
/// Index I/O.
pub fn define_view(db: &Db, def: &ViewDef) -> Result<(), CatalogError> {
    if let Some(name) = &def.profile
        && crate::profiles::profile(name).is_none()
    {
        return Err(CatalogError::UnknownProfile(name.clone()));
    }
    let mut pairs = vec![
        (DEFKEY_PROVIDER, Value::Text(def.provider.clone())),
        (DEFKEY_SYSTEM, Value::Text(def.system.clone())),
        (DEFKEY_TEMPLATE, Value::Text(def.template.clone())),
    ];
    if let Some(policy) = &def.selection {
        pairs.push((
            DEFKEY_SELECTION,
            Value::Uint(if policy.strict { 2 } else { 1 }),
        ));
        pairs.push((
            DEFKEY_REGIONS,
            Value::Array(policy.regions.iter().cloned().map(Value::Text).collect()),
        ));
        pairs.push((
            DEFKEY_LANGS,
            Value::Array(policy.langs.iter().cloned().map(Value::Text).collect()),
        ));
    }
    if let Some(profile) = &def.profile {
        pairs.push((DEFKEY_PROFILE, Value::Text(profile.clone())));
    }
    if let Some(image) = &def.image {
        pairs.push((DEFKEY_IMAGE, Value::Uint(1)));
        pairs.push((
            DEFKEY_IMAGE_CLUSTER,
            Value::Uint(u64::from(image.cluster_size)),
        ));
        pairs.push((
            DEFKEY_IMAGE_PARTITION,
            Value::Uint(u64::from(image.partition)),
        ));
        if let Some(label) = &image.label {
            pairs.push((DEFKEY_IMAGE_LABEL, Value::Text(label.clone())));
        }
    }
    let bytes = cbor::encode(&Value::Map(pairs)).expect("static keys");
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
    let (mut mode, mut regions, mut langs, mut profile) = (0u64, Vec::new(), Vec::new(), None);
    let (mut image_mode, mut image_cluster, mut image_partition, mut image_label) =
        (0u64, None, 1u64, None);
    for (key, value) in pairs {
        match (key, value) {
            (DEFKEY_PROVIDER, Value::Text(v)) => provider = Some(v),
            (DEFKEY_SYSTEM, Value::Text(v)) => system = Some(v),
            (DEFKEY_TEMPLATE, Value::Text(v)) => template = Some(v),
            (DEFKEY_SELECTION, Value::Uint(v)) => mode = v,
            (DEFKEY_REGIONS, Value::Array(items)) => {
                regions = decode_texts(items)?;
            }
            (DEFKEY_LANGS, Value::Array(items)) => {
                langs = decode_texts(items)?;
            }
            (DEFKEY_PROFILE, Value::Text(v)) => profile = Some(v),
            (DEFKEY_IMAGE, Value::Uint(v)) => image_mode = v,
            (DEFKEY_IMAGE_CLUSTER, Value::Uint(v)) => {
                image_cluster =
                    Some(u32::try_from(v).map_err(|_| CatalogError::Corrupt("view def"))?);
            }
            (DEFKEY_IMAGE_PARTITION, Value::Uint(v)) => image_partition = v,
            (DEFKEY_IMAGE_LABEL, Value::Text(v)) => image_label = Some(v),
            _ => return Err(CatalogError::Corrupt("view def")),
        }
    }
    let selection = match mode {
        0 => None,
        1 | 2 => Some(SelectionPolicy {
            regions,
            langs,
            strict: mode == 2,
        }),
        _ => return Err(CatalogError::Corrupt("view def")),
    };
    let image = match image_mode {
        0 => None,
        1 => {
            let defaults = crate::image::ImageParams::default();
            Some(crate::image::ImageParams {
                cluster_size: image_cluster.unwrap_or(defaults.cluster_size),
                partition: match image_partition {
                    0 => false,
                    1 => true,
                    _ => return Err(CatalogError::Corrupt("view def")),
                },
                label: image_label,
            })
        }
        _ => return Err(CatalogError::Corrupt("view def")),
    };
    Ok(Some(ViewDef {
        name: name.to_owned(),
        provider: provider.ok_or(CatalogError::Corrupt("view def"))?,
        system: system.ok_or(CatalogError::Corrupt("view def"))?,
        template: template.ok_or(CatalogError::Corrupt("view def"))?,
        selection,
        profile,
        image,
    }))
}

fn decode_texts(items: Vec<Value>) -> Result<Vec<String>, CatalogError> {
    items
        .into_iter()
        .map(|v| match v {
            Value::Text(s) => Ok(s),
            _ => Err(CatalogError::Corrupt("view def")),
        })
        .collect()
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
    /// 1G1R only: clone families the selector chose from.
    pub families: Option<usize>,
    /// Rows dropped because the profile's size cap can't hold them.
    pub skipped_oversize: usize,
    /// Directories that got an alpha-bucket level because they exceeded
    /// the profile's entry cap (80-views.md mitigation).
    pub bucketed_dirs: usize,
    /// Directories STILL exceeding the entry cap after bucketing
    /// (rows kept; the report is the remedy).
    pub overfull_dirs: usize,
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

    // 1G1R: resolve clone families over the whole revision and keep one
    // entry per family (held-first or strict per D57 — see
    // crate::selection). A linked retool clonelist refines families in
    // both modes.
    let (selected, families): (Option<HashSet<i64>>, Option<usize>) = match &def.selection {
        None => (None, None),
        Some(policy) => {
            let candidates = load_candidates(db, revision_id)?;
            let clonelist =
                crate::clonelist::load_clonelist(db, store, &def.provider, &def.system)?;
            let picked = select_1g1r(&candidates, policy, clonelist.as_ref());
            let count = picked.len();
            (Some(picked), Some(count))
        }
    };

    // Profile checked at definition time; a stored def naming a profile
    // this build doesn't know is a config error, not corruption.
    let profile = def
        .profile
        .as_deref()
        .map(|name| {
            crate::profiles::profile(name)
                .ok_or_else(|| CatalogError::UnknownProfile(name.to_owned()))
        })
        .transpose()?;

    // Every required claim of the current revision with a have(verified)
    // identity, resolved to a deterministic blob (smallest hash wins when
    // several blobs share the identity).
    struct Picked {
        entry_id: i64,
        entry: String,
        claim: String,
        hash: Blake3,
        size: u64,
    }
    let mut picked: Vec<Picked> = Vec::new();
    {
        let conn = db.cache();
        let mut stmt = conn.prepare(
            "SELECT e.entry_id, e.name, rc.name, MIN(b.hash), MAX(b.size)
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
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, [u8; 32]>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })?;
        for row in rows {
            let (entry_id, entry, claim, hash, size) = row?;
            picked.push(Picked {
                entry_id,
                entry,
                claim,
                hash: Blake3(hash),
                size: u64::try_from(size.unwrap_or(0)).unwrap_or(0),
            });
        }
    }

    // Layout + collision handling. Sanitized expansion first, then the
    // profile's device constraints; identical paths get a deterministic
    // ` (xxxxxxxx)` hash suffix.
    let mut rows: Vec<ViewRow> = Vec::new();
    let mut disambiguated = 0usize;
    let mut skipped_oversize = 0usize;
    let mut seen = std::collections::HashSet::new();
    for p in &picked {
        if let Some(selected) = &selected
            && !selected.contains(&p.entry_id)
        {
            continue;
        }
        if let Some(profile) = profile
            && let Some(cap) = profile.max_file_size
            && p.size > cap
        {
            // The target filesystem cannot hold this file at all;
            // auto-split is image-synthesis-era work (80-views.md).
            skipped_oversize += 1;
            continue;
        }
        let mut path = def
            .template
            .replace("{entry}", &sanitize_component(&p.entry))
            .replace("{name}", &sanitize_name(&p.claim))
            .replace("{alpha_bucket}", &alpha_bucket(&p.claim));
        if !path_is_canonical(&path) {
            path = sanitize_name(&path);
        }
        if let Some(profile) = profile {
            path = profile.constrain_path(&path);
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

    // Entry-cap mitigation (80-views.md): file rows sitting directly in
    // an over-cap directory move down one alpha-bucket level (`A`..`Z`,
    // `#` for the rest) — mitigated, not just reported. What remains
    // overfull after bucketing (subdir-heavy layouts) is reported.
    let mut bucketed_dirs = 0usize;
    let mut overfull_dirs = 0usize;
    if let Some(profile) = profile
        && let Some(cap) = profile.max_dir_entries
    {
        (bucketed_dirs, overfull_dirs) = bucket_overfull_dirs(&mut rows, cap, &mut disambiguated);
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
    let encoded = snap
        .encode()
        .map_err(|_| CatalogError::Corrupt("viewsnap"))?;
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
        families,
        skipped_oversize,
        bucketed_dirs,
        overfull_dirs,
    })
}

/// The 80-views.md entry-cap mitigation: file rows sitting directly in
/// an over-cap directory move down one alpha-bucket level; bucketed
/// paths that collide with pre-existing rows get the deterministic
/// hash-suffix remedy (or stay put if even that collides). Returns
/// `(dirs bucketed, dirs still over cap)`.
fn bucket_overfull_dirs(
    rows: &mut [ViewRow],
    cap: usize,
    disambiguated: &mut usize,
) -> (usize, usize) {
    let overfull: HashSet<String> = dir_children(rows)
        .into_iter()
        .filter(|(_, c)| c.len() > cap)
        .map(|(p, _)| p)
        .collect();
    if !overfull.is_empty() {
        let mut seen: HashSet<String> = rows.iter().map(|r| r.path.clone()).collect();
        for row in rows.iter_mut() {
            let (parent, leaf) = match row.path.rsplit_once('/') {
                Some((p, l)) => (p.to_owned(), l.to_owned()),
                None => (String::new(), row.path.clone()),
            };
            if !overfull.contains(&parent) {
                continue;
            }
            let bucket = alpha_bucket(&leaf);
            let mut candidate = if parent.is_empty() {
                format!("{bucket}/{leaf}")
            } else {
                format!("{parent}/{bucket}/{leaf}")
            };
            seen.remove(&row.path);
            if !seen.insert(candidate.clone()) {
                let tag = &row.hash.to_hex()[..8];
                candidate = match candidate.rsplit_once('.') {
                    Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({tag}).{ext}"),
                    _ => format!("{candidate} ({tag})"),
                };
                if !seen.insert(candidate.clone()) {
                    seen.insert(row.path.clone());
                    continue; // keep the un-bucketed path
                }
                *disambiguated += 1;
            }
            row.path = candidate;
        }
    }
    let still_overfull = dir_children(rows)
        .into_iter()
        .filter(|(_, c)| c.len() > cap)
        .count();
    (overfull.len(), still_overfull)
}

/// Immediate children (files and subdirs) per directory; `""` is root.
fn dir_children(rows: &[ViewRow]) -> HashMap<String, HashSet<String>> {
    let mut children: HashMap<String, HashSet<String>> = HashMap::new();
    for row in rows {
        let mut node = row.path.as_str();
        loop {
            match node.rsplit_once('/') {
                Some((parent, leaf)) => {
                    children
                        .entry(parent.to_owned())
                        .or_default()
                        .insert(leaf.to_owned());
                    node = parent;
                }
                None => {
                    children
                        .entry(String::new())
                        .or_default()
                        .insert(node.to_owned());
                    break;
                }
            }
        }
    }
    children
}

/// `A`–`Z` from a name's FIRST character (uppercased); `#` for digits
/// and everything else — the 80-views.md bucket vocabulary. First
/// character, not first letter: device menus sort by leading char, and
/// the bucket must agree with where the eye looks.
fn alpha_bucket(name: &str) -> String {
    let leaf = name.rsplit('/').next().unwrap_or(name);
    match leaf.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase().to_string(),
        _ => "#".to_owned(),
    }
}

/// Load the revision's entries with clone links and holding status —
/// the 1G1R selector's input.
fn load_candidates(db: &Db, revision_id: i64) -> Result<Vec<Candidate>, CatalogError> {
    let conn = db.cache();
    let mut stmt = conn.prepare(
        "SELECT e.entry_id, e.name, e.cloneof_id,
           (SELECT COUNT(*) FROM rom_claim rc
             WHERE rc.entry_id = e.entry_id AND rc.status != 2 AND NOT rc.optional),
           (SELECT COUNT(*) FROM rom_claim rc
             WHERE rc.entry_id = e.entry_id AND rc.status != 2 AND NOT rc.optional
               AND EXISTS (
                 SELECT 1 FROM identity_status s
                 JOIN identity_blob ib
                   ON ib.identity_id = s.identity_id AND ib.basis >= 1
                 WHERE s.identity_id = rc.identity_id AND s.state = 4))
         FROM entry e
         WHERE e.revision_id = ?1",
    )?;
    let rows = stmt.query_map(params![revision_id], |row| {
        Ok(Candidate {
            entry_id: row.get(0)?,
            name: row.get(1)?,
            cloneof_id: row.get(2)?,
            required: row.get::<_, i64>(3)?.max(0).unsigned_abs(),
            held: row.get::<_, i64>(4)?.max(0).unsigned_abs(),
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
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
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' {
                '_'
            } else {
                c
            }
        })
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

    fn row(path: &str) -> ViewRow {
        ViewRow {
            path: path.to_owned(),
            hash: Blake3::compute(path.as_bytes()),
            size: 1,
            seek: 0,
        }
    }

    #[test]
    fn alpha_buckets_are_letters_or_hash() {
        assert_eq!(alpha_bucket("Alpha (USA).gba"), "A");
        assert_eq!(alpha_bucket("zelda.gba"), "Z");
        assert_eq!(alpha_bucket("1942.nes"), "#");
        assert_eq!(alpha_bucket("~weird~.bin"), "#");
        assert_eq!(alpha_bucket("dir/beta.gba"), "B", "buckets by leaf");
    }

    #[test]
    fn overfull_dirs_get_bucketed_not_just_reported() {
        // Cap 3: root holds four children (three files + the `B` dir);
        // bucketing brings it to three bucket dirs. One bucketed row
        // collides with the pre-existing nested path.
        let mut rows = vec![
            row("Alpha.gba"),
            row("Beta.gba"),
            row("Charlie.gba"),
            row("B/Beta.gba"), // pre-existing path at the bucket target
        ];
        let mut disambiguated = 0;
        let (bucketed, still) = bucket_overfull_dirs(&mut rows, 3, &mut disambiguated);
        assert_eq!(bucketed, 1, "root was over cap");
        assert_eq!(still, 0, "mitigated");
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"A/Alpha.gba"), "{paths:?}");
        assert!(paths.contains(&"C/Charlie.gba"), "{paths:?}");
        assert!(
            paths.contains(&"B/Beta.gba"),
            "nested row untouched: {paths:?}"
        );
        // The colliding move got the hash-suffix remedy, not a drop.
        assert_eq!(disambiguated, 1);
        assert_eq!(rows.len(), 4, "no rows lost");
        let unique: HashSet<&str> = paths.iter().copied().collect();
        assert_eq!(unique.len(), 4, "paths stay unique: {paths:?}");
        // Determinism: same input, same output.
        let mut rows2 = vec![
            row("Alpha.gba"),
            row("Beta.gba"),
            row("Charlie.gba"),
            row("B/Beta.gba"),
        ];
        let mut d2 = 0;
        bucket_overfull_dirs(&mut rows2, 3, &mut d2);
        assert_eq!(
            rows.iter().map(|r| &r.path).collect::<Vec<_>>(),
            rows2.iter().map(|r| &r.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn under_cap_dirs_are_untouched() {
        let mut rows = vec![row("a/x.gba"), row("a/y.gba"), row("b/z.gba")];
        let mut disambiguated = 0;
        let (bucketed, still) = bucket_overfull_dirs(&mut rows, 10, &mut disambiguated);
        assert_eq!((bucketed, still, disambiguated), (0, 0, 0));
        assert_eq!(rows[0].path, "a/x.gba");
    }
}
