//! Blob index, alias table (D22), and the rescan cache (RomVault lesson:
//! rescans are O(changed)).

use datboi_core::alias::AliasTuple;
use datboi_core::hash::Blake3;
use rusqlite::{OptionalExtension, params};

use crate::types::{AliasAlgo, Namespace, Residency};
use crate::{Db, IndexError};

/// A full blob-index row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRow {
    pub blob_id: i64,
    pub hash: Blake3,
    pub size: Option<u64>,
    pub namespace: Namespace,
    pub residency: Residency,
}

impl Db {
    /// Insert or update a blob row; returns its surrogate id. Size (when
    /// known) and residency are refreshed; namespace is first-write-wins
    /// (a hash never legitimately changes namespace).
    pub fn upsert_blob(
        &self,
        hash: &Blake3,
        size: Option<u64>,
        ns: Namespace,
        residency: Residency,
    ) -> Result<i64, IndexError> {
        let id = self.cache().query_row(
            "INSERT INTO blob (hash, size, namespace, residency)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(hash) DO UPDATE SET
               size = COALESCE(excluded.size, blob.size),
               residency = excluded.residency
             RETURNING blob_id",
            params![
                hash.0.as_slice(),
                size.map(i64::try_from).transpose().expect("size fits i64"),
                ns.code(),
                residency.code()
            ],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Full blob row by hash (executor route planning, eviction).
    pub fn blob_by_hash(&self, hash: &Blake3) -> Result<Option<BlobRow>, IndexError> {
        self.blob_row(
            "SELECT blob_id, hash, size, namespace, residency FROM blob WHERE hash = ?1",
            params![hash.0.as_slice()],
        )
    }

    /// Full blob row by surrogate id.
    pub fn blob_by_id(&self, blob_id: i64) -> Result<Option<BlobRow>, IndexError> {
        self.blob_row(
            "SELECT blob_id, hash, size, namespace, residency FROM blob WHERE blob_id = ?1",
            params![blob_id],
        )
    }

    fn blob_row(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Option<BlobRow>, IndexError> {
        let row = self
            .cache()
            .query_row(sql, params, |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, [u8; 32]>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .optional()?;
        row.map(|(blob_id, hash, size, ns, residency)| {
            Ok(BlobRow {
                blob_id,
                hash: Blake3(hash),
                size: size.map(|s| u64::try_from(s).expect("sizes stored non-negative")),
                namespace: Namespace::from_code(ns)?,
                residency: Residency::from_code(residency)?,
            })
        })
        .transpose()
    }

    /// Flip a blob's residency (the eviction/rematerialization state
    /// change). The caller is responsible for the D25 safety rules; this
    /// is a plain cache write.
    pub fn set_residency(&self, blob_id: i64, residency: Residency) -> Result<(), IndexError> {
        self.cache().execute(
            "UPDATE blob SET residency = ?2 WHERE blob_id = ?1",
            params![blob_id, residency.code()],
        )?;
        Ok(())
    }

    pub fn get_blob_id(&self, hash: &Blake3) -> Result<Option<i64>, IndexError> {
        Ok(self
            .cache()
            .query_row(
                "SELECT blob_id FROM blob WHERE hash = ?1",
                params![hash.0.as_slice()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Record a successful full-hash verification (ingest or scrub).
    pub fn set_verified(&self, blob_id: i64, at_unix: i64) -> Result<(), IndexError> {
        self.cache().execute(
            "UPDATE blob SET verified_at = ?2 WHERE blob_id = ?1",
            params![blob_id, at_unix],
        )?;
        Ok(())
    }

    /// Store all alias digests for a blob. Idempotent; multi-hit rows are
    /// legal by design (D2: colliding sha1/md5/crc map to many blobs).
    pub fn insert_aliases(&self, blob_id: i64, aliases: &AliasTuple) -> Result<(), IndexError> {
        let mut stmt = self.cache().prepare_cached(
            "INSERT OR IGNORE INTO alias (algo, digest, blob_id) VALUES (?1, ?2, ?3)",
        )?;
        stmt.execute(params![
            AliasAlgo::Crc32.code(),
            aliases.crc32.as_slice(),
            blob_id
        ])?;
        stmt.execute(params![
            AliasAlgo::Md5.code(),
            aliases.md5.as_slice(),
            blob_id
        ])?;
        stmt.execute(params![
            AliasAlgo::Sha1.code(),
            aliases.sha1.as_slice(),
            blob_id
        ])?;
        stmt.execute(params![
            AliasAlgo::Sha256.code(),
            aliases.sha256.as_slice(),
            blob_id
        ])?;
        Ok(())
    }

    /// Record a CHD header's declared internal sha1 for a stored blob
    /// (AliasAlgo::ChdSha1 — see the enum docs for why this is a separate
    /// namespace from real sha1 aliases).
    pub fn insert_declared_chd_sha1(
        &self,
        blob_id: i64,
        sha1: &[u8; 20],
    ) -> Result<(), IndexError> {
        self.cache().execute(
            "INSERT OR IGNORE INTO alias (algo, digest, blob_id) VALUES (?1, ?2, ?3)",
            params![AliasAlgo::ChdSha1.code(), sha1.as_slice(), blob_id],
        )?;
        Ok(())
    }

    /// All blobs a dat-hash digest resolves to (multi-hit tolerant; the
    /// caller matches on the dat's full hash set, D2).
    pub fn alias_lookup(&self, algo: AliasAlgo, digest: &[u8]) -> Result<Vec<i64>, IndexError> {
        let mut stmt = self
            .cache()
            .prepare_cached("SELECT blob_id FROM alias WHERE algo = ?1 AND digest = ?2")?;
        let ids = stmt
            .query_map(params![algo.code(), digest], |row| row.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        Ok(ids)
    }

    /// Every data/ blob with a complete alias tuple, for the state
    /// snapshot's sharded alias batches (D22). data/ only: the table exists
    /// to map dat hashes to content, and meta objects never appear in dats —
    /// including them would also make every snapshot dirty its own alias
    /// shards (the batch blobs it mints), defeating cross-snapshot dedup.
    /// Complete-tuple-only: partial rows are re-derivable and not worth a
    /// lossy encoding. Ordered by hash for determinism.
    pub fn list_alias_tuples(&self) -> Result<Vec<AliasTuple>, IndexError> {
        let mut stmt = self.cache().prepare_cached(
            "SELECT b.hash, b.size,
                    (SELECT digest FROM alias WHERE blob_id = b.blob_id AND algo = ?1),
                    (SELECT digest FROM alias WHERE blob_id = b.blob_id AND algo = ?2),
                    (SELECT digest FROM alias WHERE blob_id = b.blob_id AND algo = ?3),
                    (SELECT digest FROM alias WHERE blob_id = b.blob_id AND algo = ?4)
             FROM blob b
             WHERE b.size IS NOT NULL AND b.namespace = ?5
             ORDER BY b.hash",
        )?;
        let rows = stmt
            .query_map(
                params![
                    AliasAlgo::Crc32.code(),
                    AliasAlgo::Md5.code(),
                    AliasAlgo::Sha1.code(),
                    AliasAlgo::Sha256.code(),
                    Namespace::Data.code()
                ],
                |row| {
                    let hash: [u8; 32] = row.get(0)?;
                    let size: i64 = row.get(1)?;
                    let crc32: Option<[u8; 4]> = row.get(2)?;
                    let md5: Option<[u8; 16]> = row.get(3)?;
                    let sha1: Option<[u8; 20]> = row.get(4)?;
                    let sha256: Option<[u8; 32]> = row.get(5)?;
                    Ok((hash, size, crc32, md5, sha1, sha256))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(hash, size, crc32, md5, sha1, sha256)| {
                Some(AliasTuple {
                    blake3: Blake3(hash),
                    size: u64::try_from(size).ok()?,
                    crc32: crc32?,
                    md5: md5?,
                    sha1: sha1?,
                    sha256: sha256?,
                })
            })
            .collect())
    }

    /// Record what a source path hashed to (rescan cache).
    pub fn upsert_source_file(
        &self,
        path: &str,
        mtime_ns: i64,
        size: u64,
        blob_id: Option<i64>,
        scanned_at: i64,
    ) -> Result<(), IndexError> {
        self.cache().execute(
            "INSERT INTO source_file (path, mtime_ns, size, blob_id, scanned_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
               mtime_ns = excluded.mtime_ns,
               size = excluded.size,
               blob_id = excluded.blob_id,
               scanned_at = excluded.scanned_at",
            params![
                path,
                mtime_ns,
                i64::try_from(size).expect("size fits i64"),
                blob_id,
                scanned_at
            ],
        )?;
        Ok(())
    }

    /// The known blob for `path` iff mtime+size still match — the
    /// O(changed) fast path. `None` means the file must be re-hashed.
    pub fn lookup_unchanged_source(
        &self,
        path: &str,
        mtime_ns: i64,
        size: u64,
    ) -> Result<Option<i64>, IndexError> {
        Ok(self
            .cache()
            .query_row(
                "SELECT blob_id FROM source_file
                 WHERE path = ?1 AND mtime_ns = ?2 AND size = ?3 AND blob_id IS NOT NULL",
                params![path, mtime_ns, i64::try_from(size).expect("size fits i64")],
                |row| row.get(0),
            )
            .optional()?)
    }
}
