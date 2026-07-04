//! Blob index, alias table (D22), and the rescan cache (RomVault lesson:
//! rescans are O(changed)).

use datboi_core::alias::AliasTuple;
use datboi_core::hash::Blake3;
use rusqlite::{OptionalExtension, params};

use crate::types::{AliasAlgo, Namespace, Residency};
use crate::{Db, IndexError};

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
