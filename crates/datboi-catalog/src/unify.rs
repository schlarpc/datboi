//! Content-identity unification (D2, docs/dats.md): many claims → one
//! identity → (hopefully) one stored blob.
//!
//! Rules: claims unify iff no hash field conflicts AND a strong hash
//! (md5-or-better, per dats open-question 2's resolution) matches;
//! crc32+size-only agreement is `probable` evidence (D39), never
//! authoritative. Same sha1 with different md5 = two identities (sha1
//! collisions are legal, D2). A claim carrying hashes that bridge two
//! previously-separate compatible identities merges them (claims and blob
//! links repoint to the survivor).

use rusqlite::{Connection, OptionalExtension, params};

use datboi_index::{AliasAlgo, Db};

use crate::CatalogError;

/// Evidence strength / identity_blob basis codes (schema.md §2):
/// 3=sha256, 2=sha1, 1=md5, 0=crc32+size (probable).
pub const BASIS_SHA256: i64 = 3;
pub const BASIS_SHA1: i64 = 2;
pub const BASIS_MD5: i64 = 1;
pub const BASIS_CRC_SIZE: i64 = 0;
/// A container header's self-declaration (CHD internal sha1, D44):
/// evidence about content we never hashed ourselves. Grades as `probable`
/// in rollups, exactly like crc+size — the declaration is checkable only
/// by decompressing (M3, post-D50).
pub const BASIS_DECLARED: i64 = -1;

/// The partial hash tuple of a claim or identity row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Tuple {
    size: Option<i64>,
    crc32: Option<Vec<u8>>,
    md5: Option<Vec<u8>>,
    sha1: Option<Vec<u8>>,
    sha256: Option<Vec<u8>>,
}

impl Tuple {
    fn strength(&self) -> Option<i64> {
        if self.sha256.is_some() {
            Some(BASIS_SHA256)
        } else if self.sha1.is_some() {
            Some(BASIS_SHA1)
        } else if self.md5.is_some() {
            Some(BASIS_MD5)
        } else if self.crc32.is_some() && self.size.is_some() {
            Some(BASIS_CRC_SIZE)
        } else {
            None // nothing to identify by (bare nodump etc.)
        }
    }

    /// No overlapping field disagrees.
    fn compatible(&self, other: &Self) -> bool {
        fn ok<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
        }
        ok(&self.size, &other.size)
            && ok(&self.crc32, &other.crc32)
            && ok(&self.md5, &other.md5)
            && ok(&self.sha1, &other.sha1)
            && ok(&self.sha256, &other.sha256)
    }

    /// Do the tuples share at least one *strong* (md5+) hash value —
    /// or, failing that, agree on crc32+size (probable-grade)?
    fn match_grade(&self, other: &Self) -> Option<i64> {
        fn eq<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> bool {
            matches!((a, b), (Some(a), Some(b)) if a == b)
        }
        if eq(&self.sha256, &other.sha256) {
            Some(BASIS_SHA256)
        } else if eq(&self.sha1, &other.sha1) {
            Some(BASIS_SHA1)
        } else if eq(&self.md5, &other.md5) {
            Some(BASIS_MD5)
        } else if eq(&self.crc32, &other.crc32) && eq(&self.size, &other.size) {
            Some(BASIS_CRC_SIZE)
        } else {
            None
        }
    }

    fn absorb(&mut self, other: &Self) {
        fn fill<T: Clone>(dst: &mut Option<T>, src: &Option<T>) {
            if dst.is_none() {
                dst.clone_from(src);
            }
        }
        fill(&mut self.size, &other.size);
        fill(&mut self.crc32, &other.crc32);
        fill(&mut self.md5, &other.md5);
        fill(&mut self.sha1, &other.sha1);
        fill(&mut self.sha256, &other.sha256);
    }
}

/// Unify every claim of a revision into content identities. Returns the
/// touched identity ids (for blob linking).
pub fn unify_revision(db: &mut Db, revision_id: i64) -> Result<Vec<i64>, CatalogError> {
    // Read-then-write: must be IMMEDIATE (D93, see Db::cache_write_tx).
    let tx = db.cache_write_tx()?;
    let mut touched = Vec::new();
    {
        let mut claims = tx.prepare(
            "SELECT rc.claim_id, rc.size, rc.crc32, rc.md5, rc.sha1, rc.sha256
             FROM rom_claim rc JOIN entry e ON e.entry_id = rc.entry_id
             WHERE e.revision_id = ?1 AND rc.identity_id IS NULL
             ORDER BY rc.claim_id",
        )?;
        let rows: Vec<(i64, Tuple)> = claims
            .query_map([revision_id], |row| {
                Ok((
                    row.get(0)?,
                    Tuple {
                        size: row.get(1)?,
                        crc32: row.get(2)?,
                        md5: row.get(3)?,
                        sha1: row.get(4)?,
                        sha256: row.get(5)?,
                    },
                ))
            })?
            .collect::<Result<_, _>>()?;

        for (claim_id, tuple) in rows {
            let Some(identity_id) = unify_claim(&tx, &tuple)? else {
                continue; // nothing identifiable; identity_id stays NULL
            };
            tx.execute(
                "UPDATE rom_claim SET identity_id = ?2 WHERE claim_id = ?1",
                params![claim_id, identity_id],
            )?;
            touched.push(identity_id);
        }
    }
    tx.commit()?;
    touched.sort_unstable();
    touched.dedup();
    Ok(touched)
}

/// Find-or-create the identity for one claim tuple; merges bridged
/// identities. Returns None for unidentifiable tuples.
fn unify_claim(conn: &Connection, tuple: &Tuple) -> Result<Option<i64>, CatalogError> {
    if tuple.strength().is_none() {
        return Ok(None);
    }

    // Candidates: identities sharing any present hash value.
    let mut candidates: Vec<(i64, Tuple)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let lookups: [(&str, Option<&Vec<u8>>); 4] = [
        ("sha256", tuple.sha256.as_ref()),
        ("sha1", tuple.sha1.as_ref()),
        ("md5", tuple.md5.as_ref()),
        ("crc32", tuple.crc32.as_ref()),
    ];
    for (column, digest) in lookups {
        let Some(digest) = digest else { continue };
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT identity_id, size, crc32, md5, sha1, sha256
             FROM content_identity WHERE {column} = ?1"
        ))?;
        let rows = stmt.query_map(params![digest], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                Tuple {
                    size: row.get(1)?,
                    crc32: row.get(2)?,
                    md5: row.get(3)?,
                    sha1: row.get(4)?,
                    sha256: row.get(5)?,
                },
            ))
        })?;
        for row in rows {
            let (id, t) = row?;
            if seen.insert(id) {
                candidates.push((id, t));
            }
        }
    }

    // A candidate qualifies when compatible AND matched at a grade at
    // least as strong as the weaker of the two tuples can offer: strong
    // hashes must match on a strong hash; crc-only tuples may match on
    // crc+size (probable-grade linkage happens at the blob layer, but for
    // claim↔claim unification crc+size agreement without conflict is the
    // dats "probable" unification, accepted for identity sharing).
    let mut qualifying: Vec<(i64, Tuple)> = candidates
        .into_iter()
        .filter(|(_, t)| tuple.compatible(t) && tuple.match_grade(t).is_some())
        .collect();

    let Some((survivor_id, mut survivor_tuple)) = qualifying
        .iter()
        .max_by_key(|(id, t)| (t.strength().unwrap_or(-1), std::cmp::Reverse(*id)))
        .cloned()
    else {
        // No qualifying identity: mint one.
        let strength = tuple.strength().expect("checked above");
        let id = conn.query_row(
            "INSERT INTO content_identity (size, crc32, md5, sha1, sha256, strength)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING identity_id",
            params![
                tuple.size,
                tuple.crc32,
                tuple.md5,
                tuple.sha1,
                tuple.sha256,
                strength
            ],
            |row| row.get(0),
        )?;
        return Ok(Some(id));
    };

    // Merge any other qualifying identities into the survivor, provided
    // they are also compatible with the survivor's (claim-absorbed) tuple.
    survivor_tuple.absorb(tuple);
    qualifying.retain(|(id, _)| *id != survivor_id);
    for (loser_id, loser_tuple) in &qualifying {
        if !survivor_tuple.compatible(loser_tuple) {
            continue; // e.g. sha1 bridge but md5 conflict: stays separate
        }
        survivor_tuple.absorb(loser_tuple);
        conn.execute(
            "UPDATE rom_claim SET identity_id = ?1 WHERE identity_id = ?2",
            params![survivor_id, loser_id],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO identity_blob (identity_id, blob_id, basis)
             SELECT ?1, blob_id, basis FROM identity_blob WHERE identity_id = ?2",
            params![survivor_id, loser_id],
        )?;
        conn.execute(
            "DELETE FROM identity_blob WHERE identity_id = ?1",
            params![loser_id],
        )?;
        conn.execute(
            "DELETE FROM identity_status WHERE identity_id = ?1",
            params![loser_id],
        )?;
        conn.execute(
            "DELETE FROM content_identity WHERE identity_id = ?1",
            params![loser_id],
        )?;
    }

    let strength = survivor_tuple.strength().expect("merged tuple has hashes");
    conn.execute(
        "UPDATE content_identity
         SET size = ?2, crc32 = ?3, md5 = ?4, sha1 = ?5, sha256 = ?6, strength = ?7
         WHERE identity_id = ?1",
        params![
            survivor_id,
            survivor_tuple.size,
            survivor_tuple.crc32,
            survivor_tuple.md5,
            survivor_tuple.sha1,
            survivor_tuple.sha256,
            strength
        ],
    )?;
    Ok(Some(survivor_id))
}

/// Re-link every identity against the current alias table — for use after
/// ingest runs that post-date a dat import (import links only the
/// identities it touched).
pub fn relink_all(db: &Db) -> Result<(), CatalogError> {
    let mut stmt = db
        .cache()
        .prepare("SELECT identity_id FROM content_identity")?;
    let ids: Vec<i64> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    link_identities_to_blobs(db, &ids)
}

/// Link identities to stored blobs through the alias table. A blob
/// qualifies when every hash the identity knows matches the blob's alias
/// tuple (ingest always records all four, D2) and sizes agree; basis =
/// the identity's evidence strength.
pub fn link_identities_to_blobs(db: &Db, identity_ids: &[i64]) -> Result<(), CatalogError> {
    // Read-then-write: must be IMMEDIATE (D93, see Db::cache_write_tx).
    let tx = db.cache_write_tx()?;
    for &identity_id in identity_ids {
        let Some(tuple) = load_identity(&tx, identity_id)? else {
            continue; // merged away earlier in this import
        };
        let strength = tuple.strength().unwrap_or(BASIS_CRC_SIZE);

        // Seed candidates from the strongest available hash.
        let (algo, digest): (AliasAlgo, &[u8]) = if let Some(d) = &tuple.sha256 {
            (AliasAlgo::Sha256, d)
        } else if let Some(d) = &tuple.sha1 {
            (AliasAlgo::Sha1, d)
        } else if let Some(d) = &tuple.md5 {
            (AliasAlgo::Md5, d)
        } else if let Some(d) = &tuple.crc32 {
            (AliasAlgo::Crc32, d)
        } else {
            continue;
        };
        let candidates = db.alias_lookup(algo, digest)?;

        for blob_id in candidates {
            if blob_matches(&tx, blob_id, &tuple)? {
                tx.execute(
                    "INSERT OR IGNORE INTO identity_blob (identity_id, blob_id, basis)
                     VALUES (?1, ?2, ?3)",
                    params![identity_id, blob_id, strength],
                )?;
            }
        }

        // CHD declared-sha1 pass (D44): a sizeless sha1-bearing identity is
        // the shape of a disk claim; link any stored CHD whose header
        // declares that sha1, at declared (probable) grade. `blob_matches`
        // is deliberately skipped — the declaration describes decompressed
        // content, so the blob's real alias tuple can never corroborate it.
        if let (Some(sha1), None) = (&tuple.sha1, &tuple.size) {
            for blob_id in db.alias_lookup(AliasAlgo::ChdSha1, sha1)? {
                tx.execute(
                    "INSERT OR IGNORE INTO identity_blob (identity_id, blob_id, basis)
                     VALUES (?1, ?2, ?3)",
                    params![identity_id, blob_id, BASIS_DECLARED],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

fn load_identity(conn: &Connection, identity_id: i64) -> Result<Option<Tuple>, CatalogError> {
    Ok(conn
        .query_row(
            "SELECT size, crc32, md5, sha1, sha256 FROM content_identity
             WHERE identity_id = ?1",
            [identity_id],
            |row| {
                Ok(Tuple {
                    size: row.get(0)?,
                    crc32: row.get(1)?,
                    md5: row.get(2)?,
                    sha1: row.get(3)?,
                    sha256: row.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Every hash the identity carries must match one of the blob's alias
/// rows; identity size (if known) must equal blob size.
fn blob_matches(conn: &Connection, blob_id: i64, tuple: &Tuple) -> Result<bool, CatalogError> {
    if let Some(size) = tuple.size {
        let blob_size: Option<i64> = conn.query_row(
            "SELECT size FROM blob WHERE blob_id = ?1",
            [blob_id],
            |row| row.get(0),
        )?;
        if blob_size != Some(size) {
            return Ok(false);
        }
    }
    let checks: [(AliasAlgo, Option<&Vec<u8>>); 4] = [
        (AliasAlgo::Crc32, tuple.crc32.as_ref()),
        (AliasAlgo::Md5, tuple.md5.as_ref()),
        (AliasAlgo::Sha1, tuple.sha1.as_ref()),
        (AliasAlgo::Sha256, tuple.sha256.as_ref()),
    ];
    for (algo, digest) in checks {
        let Some(digest) = digest else { continue };
        let hit: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM alias WHERE algo = ?1 AND digest = ?2 AND blob_id = ?3",
                params![algo.code(), digest, blob_id],
                |row| row.get(0),
            )
            .optional()?;
        if hit.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}
