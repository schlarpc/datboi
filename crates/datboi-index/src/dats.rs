//! Dat model tables (docs/dats.md → schema.md §2). Attrs/header
//! long-tail data is stored as SQLite JSONB (preserved-not-queried; the
//! CAS dat blob is the true canonical form; generated columns are the
//! index-later escape hatch).

use datboi_core::hash::Blake3;
use rusqlite::params;

use crate::types::{ClaimKind, ClaimStatus};
use crate::{Db, IndexError};

pub struct NewRelease<'a> {
    pub name: &'a str,
    pub region: &'a str,
    pub language: Option<&'a str>,
    pub date: Option<&'a str>,
    pub is_default: bool,
}

pub struct NewClaim<'a> {
    pub kind: ClaimKind,
    pub name: &'a str,
    pub size: Option<u64>,
    pub crc32: Option<[u8; 4]>,
    pub md5: Option<[u8; 16]>,
    pub sha1: Option<[u8; 20]>,
    pub sha256: Option<[u8; 32]>,
    pub status: ClaimStatus,
    pub mia: bool,
    pub optional: bool,
    pub merge_name: Option<&'a str>,
    /// JSON text; stored as JSONB.
    pub attrs: Option<&'a str>,
}

pub struct NewEntry<'a> {
    pub name: &'a str,
    /// No-Intro `game/@id` where present (revision-diff anchor).
    pub stable_key: Option<&'a str>,
    pub description: Option<&'a str>,
    pub year: Option<&'a str>,
    pub manufacturer: Option<&'a str>,
    pub is_bios: bool,
    pub is_device: bool,
    pub is_mechanical: bool,
    pub runnable: bool,
    pub cloneof: Option<&'a str>,
    pub romof: Option<&'a str>,
    pub sampleof: Option<&'a str>,
    /// JSON text; stored as JSONB.
    pub attrs: Option<&'a str>,
    pub releases: Vec<NewRelease<'a>>,
    pub claims: Vec<NewClaim<'a>>,
}

impl Db {
    /// Every source with a current revision: what a state snapshot needs to
    /// replay `dat import` after recovery (provider, system, dat blob hash,
    /// original imported_at). Ordered by (provider, system) — the snapshot
    /// payload's canonical order.
    pub fn list_current_sources(&self) -> Result<Vec<(String, String, Blake3, i64)>, IndexError> {
        let mut stmt = self.cache().prepare_cached(
            "SELECT s.provider, s.system, b.hash, r.imported_at
             FROM dat_source s
             JOIN dat_revision r ON r.revision_id = s.current_revision_id
             JOIN blob b ON b.blob_id = r.blob_id
             ORDER BY s.provider, s.system",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let provider: String = row.get(0)?;
                let system: String = row.get(1)?;
                let hash: [u8; 32] = row.get(2)?;
                let imported_at: i64 = row.get(3)?;
                Ok((provider, system, Blake3(hash), imported_at))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get-or-create a dat source; returns source_id.
    pub fn upsert_dat_source(&self, provider: &str, system: &str) -> Result<i64, IndexError> {
        // ON CONFLICT DO UPDATE (a no-op set) so RETURNING fires on both paths.
        let id = self.cache().query_row(
            "INSERT INTO dat_source (provider, system) VALUES (?1, ?2)
             ON CONFLICT(provider, system) DO UPDATE SET provider = excluded.provider
             RETURNING source_id",
            params![provider, system],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)] // one row, one insert; a struct would just rename the columns
    pub fn insert_dat_revision(
        &self,
        source_id: i64,
        blob_id: i64,
        format: i64,
        version: Option<&str>,
        dat_date: Option<&str>,
        header_json: Option<&str>,
        detector_id: Option<i64>,
        imported_at: i64,
    ) -> Result<i64, IndexError> {
        let id = self.cache().query_row(
            "INSERT INTO dat_revision
               (source_id, blob_id, format, version, dat_date, header, detector_id, imported_at)
             VALUES (?1, ?2, ?3, ?4, ?5, jsonb(?6), ?7, ?8)
             RETURNING revision_id",
            params![
                source_id,
                blob_id,
                format,
                version,
                dat_date,
                header_json,
                detector_id,
                imported_at
            ],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Flip the source's "current" pointer (dats: revisions are
    /// immutable; currency is a pointer).
    pub fn set_current_revision(&self, source_id: i64, revision_id: i64) -> Result<(), IndexError> {
        self.cache().execute(
            "UPDATE dat_source SET current_revision_id = ?2 WHERE source_id = ?1",
            params![source_id, revision_id],
        )?;
        Ok(())
    }

    /// Bulk-insert a revision's entries, releases, and claims in one
    /// transaction with prepared statements (per-row transactions are
    /// death at MAME scale), then resolve intra-revision cloneof/romof
    /// name refs to entry ids.
    pub fn insert_entries(
        &mut self,
        revision_id: i64,
        entries: &[NewEntry<'_>],
    ) -> Result<u64, IndexError> {
        let tx = self.cache.transaction()?;
        {
            let mut entry_stmt = tx.prepare_cached(
                "INSERT INTO entry (revision_id, name, stable_key, description, year,
                   manufacturer, is_bios, is_device, is_mechanical, runnable,
                   cloneof, romof, sampleof, attrs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, jsonb(?14))",
            )?;
            let mut release_stmt = tx.prepare_cached(
                "INSERT INTO release (entry_id, name, region, language, rel_date, is_default)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let mut claim_stmt = tx.prepare_cached(
                "INSERT INTO rom_claim (entry_id, kind, name, size, crc32, md5, sha1, sha256,
                   status, mia, optional, merge_name, attrs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, jsonb(?13))",
            )?;
            for entry in entries {
                entry_stmt.execute(params![
                    revision_id,
                    entry.name,
                    entry.stable_key,
                    entry.description,
                    entry.year,
                    entry.manufacturer,
                    entry.is_bios,
                    entry.is_device,
                    entry.is_mechanical,
                    entry.runnable,
                    entry.cloneof,
                    entry.romof,
                    entry.sampleof,
                    entry.attrs
                ])?;
                let entry_id = tx.last_insert_rowid();
                for release in &entry.releases {
                    release_stmt.execute(params![
                        entry_id,
                        release.name,
                        release.region,
                        release.language,
                        release.date,
                        release.is_default
                    ])?;
                }
                for claim in &entry.claims {
                    claim_stmt.execute(params![
                        entry_id,
                        claim.kind.code(),
                        claim.name,
                        claim.size.map(|s| i64::try_from(s).expect("size fits i64")),
                        claim.crc32.as_ref().map(<[u8; 4]>::as_slice),
                        claim.md5.as_ref().map(<[u8; 16]>::as_slice),
                        claim.sha1.as_ref().map(<[u8; 20]>::as_slice),
                        claim.sha256.as_ref().map(<[u8; 32]>::as_slice),
                        claim.status.code(),
                        claim.mia,
                        claim.optional,
                        claim.merge_name,
                        claim.attrs
                    ])?;
                }
            }
            // Resolve parent names within the revision (unresolvable names
            // stay NULL — dats reference missing parents in the wild).
            for column in ["cloneof", "romof"] {
                tx.execute(
                    &format!(
                        "UPDATE entry SET {column}_id = (
                           SELECT p.entry_id FROM entry p
                           WHERE p.revision_id = ?1 AND p.name = entry.{column})
                         WHERE revision_id = ?1 AND {column} IS NOT NULL"
                    ),
                    params![revision_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(entries.len() as u64)
    }
}
