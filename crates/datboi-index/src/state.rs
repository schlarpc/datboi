//! state.db surface (D37): the authoritative-until-snapshotted remainder.
//! Everything here (except sessions) round-trips through the signed CAS
//! state snapshot (D15).

use datboi_core::hash::Blake3;
use rusqlite::{OptionalExtension, params};

use crate::{Db, IndexError};

impl Db {
    /// Create or move a tag (GC root / pin).
    pub fn set_tag(&self, name: &str, hash: &Blake3, created_at: i64) -> Result<(), IndexError> {
        self.state().execute(
            "INSERT INTO tag (name, hash, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET hash = excluded.hash,
               created_at = excluded.created_at",
            params![name, hash.0.as_slice(), created_at],
        )?;
        Ok(())
    }

    pub fn get_tag(&self, name: &str) -> Result<Option<Blake3>, IndexError> {
        Ok(self
            .state()
            .query_row(
                "SELECT hash FROM tag WHERE name = ?1",
                params![name],
                |row| {
                    let bytes: [u8; 32] = row.get(0)?;
                    Ok(Blake3(bytes))
                },
            )
            .optional()?)
    }

    pub fn delete_tag(&self, name: &str) -> Result<bool, IndexError> {
        Ok(self
            .state()
            .execute("DELETE FROM tag WHERE name = ?1", params![name])?
            > 0)
    }

    pub fn list_tags(&self) -> Result<Vec<(String, Blake3)>, IndexError> {
        let mut stmt = self
            .state()
            .prepare_cached("SELECT name, hash FROM tag ORDER BY name")?;
        let rows = stmt
            .query_map([], |row| {
                let name: String = row.get(0)?;
                let bytes: [u8; 32] = row.get(1)?;
                Ok((name, Blake3(bytes)))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn config_set(&self, key: &str, value: &[u8]) -> Result<(), IndexError> {
        self.state().execute(
            "INSERT INTO config (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// All config rows whose key starts with `prefix` (key order).
    pub fn config_list_prefix(&self, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, IndexError> {
        let mut stmt = self.state().prepare_cached(
            "SELECT key, value FROM config WHERE substr(key, 1, length(?1)) = ?1 ORDER BY key",
        )?;
        let rows = stmt
            .query_map(params![prefix], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn config_get(&self, key: &str) -> Result<Option<Vec<u8>>, IndexError> {
        Ok(self
            .state()
            .query_row(
                "SELECT value FROM config WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// The sequence number the NEXT snapshot will get (mint the object with
    /// this, then [`Db::snapshot_log_append`] it — the log assigns the same
    /// number because seq is the INTEGER PRIMARY KEY).
    pub fn next_snapshot_seq(&self) -> Result<i64, IndexError> {
        Ok(self.state().query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM snapshot_log",
            [],
            |row| row.get(0),
        )?)
    }

    /// Re-seed the snapshot log from a recovered snapshot object (explicit
    /// seq): sequence monotonicity is authoritative state and must survive
    /// a DB nuke, or the next mint would reuse an existing sequence.
    pub fn snapshot_log_restore(
        &self,
        seq: i64,
        hash: &Blake3,
        created_at: i64,
    ) -> Result<(), IndexError> {
        self.state().execute(
            "INSERT OR REPLACE INTO snapshot_log (seq, hash, created_at) VALUES (?1, ?2, ?3)",
            params![seq, hash.0.as_slice(), created_at],
        )?;
        Ok(())
    }

    /// Append a snapshot emission record; returns its sequence number.
    /// Newest snapshot the log knows: `(hash, seq)` — the D75 cadence
    /// check reads this to fetch the last payload for comparison.
    pub fn latest_snapshot(&self) -> Result<Option<(Blake3, i64)>, IndexError> {
        use rusqlite::OptionalExtension as _;
        Ok(self
            .state()
            .query_row(
                "SELECT hash, seq FROM snapshot_log ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, [u8; 32]>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .map(|(hash, seq)| (Blake3(hash), seq)))
    }

    pub fn snapshot_log_append(&self, hash: &Blake3, created_at: i64) -> Result<i64, IndexError> {
        let seq = self.state().query_row(
            "INSERT INTO snapshot_log (hash, created_at) VALUES (?1, ?2) RETURNING seq",
            params![hash.0.as_slice(), created_at],
            |row| row.get(0),
        )?;
        Ok(seq)
    }
}
