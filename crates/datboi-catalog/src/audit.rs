//! Audit reports: the D39 six-state contract
//! (have-verified / have-claimed / probable / available-from-peer /
//! missing / unknown). Reads entry_audit and splits `probable` back out
//! of the rollup's folded `missing` (see rollup module docs). `unknown`
//! is the store-side complement (blobs matching no claim) and is reported
//! at ingest level, not per-dat here.

use rusqlite::params;

use datboi_index::Db;

use crate::CatalogError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryAudit {
    pub name: String,
    pub required: u64,
    pub have_verified: u64,
    pub have_claimed: u64,
    pub probable: u64,
    pub peer_available: u64,
    pub missing: u64,
    /// Required claims flagged mia upstream (surfaced, still counted).
    pub mia: u64,
}

impl EntryAudit {
    #[must_use]
    pub fn complete(&self) -> bool {
        self.missing == 0 && self.probable == 0
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Totals {
    pub entries: u64,
    pub entries_complete: u64,
    pub required: u64,
    pub have_verified: u64,
    pub have_claimed: u64,
    pub probable: u64,
    pub peer_available: u64,
    pub missing: u64,
    pub mia: u64,
}

#[derive(Debug)]
pub struct AuditReport {
    pub provider: String,
    pub system: String,
    pub revision_id: i64,
    pub entries: Vec<EntryAudit>,
    pub totals: Totals,
}

/// Resolve a source's current revision id.
pub fn current_revision(db: &Db, provider: &str, system: &str) -> Result<i64, CatalogError> {
    db.cache()
        .query_row(
            "SELECT current_revision_id FROM dat_source
             WHERE provider = ?1 AND system = ?2",
            params![provider, system],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => CatalogError::UnknownSource {
                provider: provider.to_owned(),
                system: system.to_owned(),
            },
            other => CatalogError::Sqlite(other),
        })?
        .ok_or_else(|| CatalogError::NoCurrentRevision {
            provider: provider.to_owned(),
            system: system.to_owned(),
        })
}

/// Audit a source's current revision (non-merged scope, D31).
pub fn audit(db: &Db, provider: &str, system: &str) -> Result<AuditReport, CatalogError> {
    let revision_id = current_revision(db, provider, system)?;
    let conn = db.cache();
    let mut stmt = conn.prepare(
        "SELECT e.name, ea.required, ea.have_verified, ea.have_claimed,
                ea.peer_avail, ea.missing,
                COALESCE((SELECT SUM(s.state = 1)
                          FROM rom_claim rc
                          LEFT JOIN identity_status s ON s.identity_id = rc.identity_id
                          WHERE rc.entry_id = e.entry_id
                            AND rc.status != 2 AND NOT rc.optional), 0),
                COALESCE((SELECT SUM(rc.mia)
                          FROM rom_claim rc
                          WHERE rc.entry_id = e.entry_id
                            AND rc.status != 2 AND NOT rc.optional), 0)
         FROM entry e
         JOIN entry_audit ea ON ea.entry_id = e.entry_id
         WHERE e.revision_id = ?1
         ORDER BY e.name",
    )?;
    let rows = stmt.query_map([revision_id], |row| {
        let probable: u64 = row.get(6)?;
        let missing_folded: u64 = row.get(5)?;
        Ok(EntryAudit {
            name: row.get(0)?,
            required: row.get(1)?,
            have_verified: row.get(2)?,
            have_claimed: row.get(3)?,
            peer_available: row.get(4)?,
            // The rollup folds probable into missing; split it back out.
            missing: missing_folded.saturating_sub(probable),
            probable,
            mia: row.get(7)?,
        })
    })?;

    let mut entries = Vec::new();
    let mut totals = Totals::default();
    for row in rows {
        let entry = row?;
        totals.entries += 1;
        totals.entries_complete += u64::from(entry.complete());
        totals.required += entry.required;
        totals.have_verified += entry.have_verified;
        totals.have_claimed += entry.have_claimed;
        totals.probable += entry.probable;
        totals.peer_available += entry.peer_available;
        totals.missing += entry.missing;
        totals.mia += entry.mia;
        entries.push(entry);
    }

    Ok(AuditReport {
        provider: provider.to_owned(),
        system: system.to_owned(),
        revision_id,
        entries,
        totals,
    })
}
