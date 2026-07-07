//! Revision diffing (docs/85-cli.md `dat diff`, D38): compare a source's
//! two newest materialized revisions and report added / removed / renamed /
//! rehashed entries.
//!
//! Matching is three passes, strongest signal first:
//!
//! 1. **stable_key** (No-Intro `game/@id`) — the ratified rename/rehash
//!    anchor: survives both a rename and a content change at once.
//! 2. **name** — the common case: same entry, possibly new hashes.
//! 3. **content fingerprint** — catches renames in dats without stable
//!    keys: an entry whose claim *content* (kind/size/digests, claim names
//!    deliberately excluded — renamed games rename their rom files too) is
//!    unique on both sides and identical across them is the same entry
//!    wearing a new name. Ambiguous fingerprints (duplicates on either
//!    side) never match — a wrong rename is worse than an added+removed
//!    pair.
//!
//! Anything left unmatched is added (new side) or removed (old side).

use datboi_core::hash::Blake3;
use datboi_index::Db;
use rusqlite::params;

use crate::CatalogError;

/// One matched entry whose claim content changed (it may have been renamed
/// in the same revision if the match came from a stable key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rehashed {
    pub name_old: String,
    pub name_new: String,
}

#[derive(Debug)]
pub struct DatDiff {
    pub provider: String,
    pub system: String,
    pub revision_old: i64,
    pub revision_new: i64,
    pub entries_old: u64,
    pub entries_new: u64,
    /// Entry names present only in the new revision.
    pub added: Vec<String>,
    /// Entry names present only in the old revision.
    pub removed: Vec<String>,
    /// (old name, new name): same content, new name.
    pub renamed: Vec<(String, String)>,
    /// Matched entries whose claim content changed.
    pub rehashed: Vec<Rehashed>,
}

impl DatDiff {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.renamed.is_empty()
            && self.rehashed.is_empty()
    }
}

struct DiffEntry {
    name: String,
    stable_key: Option<String>,
    fingerprint: Blake3,
    matched: bool,
}

/// Diff a source's two newest materialized revisions (previous → current).
///
/// # Errors
/// [`CatalogError::UnknownSource`] for an unknown source,
/// [`CatalogError::NoPreviousRevision`] when only one revision has ever
/// been imported.
pub fn diff_source(db: &Db, provider: &str, system: &str) -> Result<DatDiff, CatalogError> {
    let conn = db.cache();
    let source_id: i64 = conn
        .query_row(
            "SELECT source_id FROM dat_source WHERE provider = ?1 AND system = ?2",
            params![provider, system],
            |row| row.get(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => CatalogError::UnknownSource {
                provider: provider.to_owned(),
                system: system.to_owned(),
            },
            other => CatalogError::Sqlite(other),
        })?;

    // Two newest materialized revisions; import order == revision_id order.
    let mut stmt = conn.prepare(
        "SELECT revision_id FROM dat_revision
         WHERE source_id = ?1 AND materialized = 1
         ORDER BY revision_id DESC LIMIT 2",
    )?;
    let revisions = stmt
        .query_map([source_id], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let [revision_new, revision_old] = revisions[..] else {
        return Err(CatalogError::NoPreviousRevision {
            provider: provider.to_owned(),
            system: system.to_owned(),
        });
    };

    let mut old = load_entries(db, revision_old)?;
    let mut new = load_entries(db, revision_new)?;
    let entries_old = old.len() as u64;
    let entries_new = new.len() as u64;

    let mut pairs: Vec<(usize, usize)> = Vec::new();

    // Pass 1: stable key.
    let old_by_key: std::collections::HashMap<&str, usize> = old
        .iter()
        .enumerate()
        .filter_map(|(i, e)| Some((e.stable_key.as_deref()?, i)))
        .collect();
    for (j, entry) in new.iter().enumerate() {
        if let Some(key) = entry.stable_key.as_deref()
            && let Some(&i) = old_by_key.get(key)
            && !old[i].matched
        {
            pairs.push((i, j));
        }
    }
    mark(&mut old, &mut new, &pairs);

    // Pass 2: exact name.
    let old_by_name: std::collections::HashMap<&str, usize> = old
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.matched)
        .map(|(i, e)| (e.name.as_str(), i))
        .collect();
    let mut name_pairs = Vec::new();
    for (j, entry) in new.iter().enumerate() {
        if !entry.matched
            && let Some(&i) = old_by_name.get(entry.name.as_str())
        {
            name_pairs.push((i, j));
        }
    }
    mark(&mut old, &mut new, &name_pairs);
    pairs.extend(name_pairs);

    // Pass 3: content fingerprint, unique on both sides only.
    let unique_by_fp = |entries: &[DiffEntry]| -> std::collections::HashMap<Blake3, usize> {
        let mut counts: std::collections::HashMap<Blake3, (usize, usize)> =
            std::collections::HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            if !e.matched {
                let slot = counts.entry(e.fingerprint).or_insert((0, i));
                slot.0 += 1;
            }
        }
        counts
            .into_iter()
            .filter(|(_, (n, _))| *n == 1)
            .map(|(fp, (_, i))| (fp, i))
            .collect()
    };
    let old_fps = unique_by_fp(&old);
    let new_fps = unique_by_fp(&new);
    let mut fp_pairs = Vec::new();
    for (fp, &j) in &new_fps {
        if let Some(&i) = old_fps.get(fp) {
            fp_pairs.push((i, j));
        }
    }
    mark(&mut old, &mut new, &fp_pairs);
    pairs.extend(fp_pairs);

    // Classify.
    let mut renamed = Vec::new();
    let mut rehashed = Vec::new();
    for &(i, j) in &pairs {
        let (o, n) = (&old[i], &new[j]);
        if o.fingerprint != n.fingerprint {
            rehashed.push(Rehashed {
                name_old: o.name.clone(),
                name_new: n.name.clone(),
            });
        } else if o.name != n.name {
            renamed.push((o.name.clone(), n.name.clone()));
        }
    }
    let mut added: Vec<String> = new
        .iter()
        .filter(|e| !e.matched)
        .map(|e| e.name.clone())
        .collect();
    let mut removed: Vec<String> = old
        .iter()
        .filter(|e| !e.matched)
        .map(|e| e.name.clone())
        .collect();
    added.sort();
    removed.sort();
    renamed.sort();
    rehashed.sort_by(|a, b| a.name_new.cmp(&b.name_new));

    Ok(DatDiff {
        provider: provider.to_owned(),
        system: system.to_owned(),
        revision_old,
        revision_new,
        entries_old,
        entries_new,
        added,
        removed,
        renamed,
        rehashed,
    })
}

fn mark(old: &mut [DiffEntry], new: &mut [DiffEntry], pairs: &[(usize, usize)]) {
    for &(i, j) in pairs {
        old[i].matched = true;
        new[j].matched = true;
    }
}

/// Load a revision's entries with content fingerprints: blake3 over the
/// sorted claim rows' (kind, size, crc32, md5, sha1, sha256) — claim names
/// excluded (see module docs), all claims included regardless of status
/// (the diff reports the dat text, not audit policy).
fn load_entries(db: &Db, revision_id: i64) -> Result<Vec<DiffEntry>, CatalogError> {
    let conn = db.cache();
    let mut stmt = conn.prepare_cached(
        "SELECT e.entry_id, e.name, e.stable_key FROM entry e
         WHERE e.revision_id = ?1 ORDER BY e.entry_id",
    )?;
    let entries = stmt
        .query_map([revision_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut claim_stmt = conn.prepare_cached(
        "SELECT kind, size, crc32, md5, sha1, sha256 FROM rom_claim
         WHERE entry_id = ?1 ORDER BY kind, size, crc32, md5, sha1, sha256",
    )?;
    let mut out = Vec::with_capacity(entries.len());
    for (entry_id, name, stable_key) in entries {
        let mut hasher = blake3::Hasher::new();
        let rows = claim_stmt.query_map([entry_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
            ))
        })?;
        for row in rows {
            let (kind, size, crc32, md5, sha1, sha256) = row?;
            // Length-prefixed fields so adjacent rows can't alias.
            hasher.update(&kind.to_le_bytes());
            hasher.update(&size.unwrap_or(-1).to_le_bytes());
            for digest in [&crc32, &md5, &sha1, &sha256] {
                match digest {
                    Some(d) => {
                        hasher.update(&(d.len() as u64).to_le_bytes());
                        hasher.update(d);
                    }
                    None => {
                        hasher.update(&u64::MAX.to_le_bytes());
                    }
                }
            }
        }
        out.push(DiffEntry {
            name,
            stable_key,
            fingerprint: Blake3(*hasher.finalize().as_bytes()),
            matched: false,
        });
    }
    Ok(out)
}
