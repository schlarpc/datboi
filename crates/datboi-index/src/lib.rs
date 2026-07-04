//! Metadata DB layer (docs/65-schema.md, D37): two SQLite files on
//! daemon-local disk, never NFS (D15).
//!
//! - `cache.db` — derivable from CAS bytes + deterministic re-import;
//!   corruption remedy is delete + rescan; `synchronous=NORMAL`.
//! - `state.db` — authoritative-until-snapshotted (tags, users, views,
//!   channels, config); `synchronous=FULL`; real migrations forever.
//!
//! The split makes the rebuildability doctrine mechanical: sole truth may
//! only live in state.db, which must round-trip through the CAS snapshot
//! encoder. Cross-file consistency is eventual — recovery assumes it.

pub mod blobs;
pub mod dats;
pub mod recipes;
pub mod schema;
pub mod state;
pub mod types;

use std::path::{Path, PathBuf};

use rusqlite::Connection;

pub use recipes::GroundingMode;
pub use types::{
    AliasAlgo, ClaimKind, ClaimStatus, Namespace, OpKind, RecipeSource, Residency, SeekClass,
    VerifyState,
};

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("{file}: application_id {found:#010x} is not a datboi {file} database")]
    WrongApplicationId { file: &'static str, found: u32 },
    #[error("{file}: schema version {found} not supported (expected {expected})")]
    SchemaVersion {
        file: &'static str,
        found: u32,
        expected: u32,
    },
    #[error("illegal recipe verify transition {from:?} -> {to:?}")]
    IllegalTransition { from: VerifyState, to: VerifyState },
    #[error("recipe {0} not found")]
    RecipeNotFound(i64),
    #[error("invalid {what} code {code} in database")]
    Decode { what: &'static str, code: i64 },
}

pub struct Db {
    cache: Connection,
    state: Connection,
    cache_path: PathBuf,
}

impl Db {
    /// Open (creating if absent) both database files inside `dir`.
    ///
    /// D15 recovery ordering note: a cache rebuild needs state.db (or the
    /// latest CAS snapshot) first — tags and the dat-source list decide
    /// what gets re-imported. [`Db::open`] therefore opens state first.
    pub fn open(dir: &Path) -> Result<Self, IndexError> {
        let state_path = dir.join("state.db");
        let cache_path = dir.join("cache.db");
        let state = open_file(
            &state_path,
            "state.db",
            schema::STATE_APP_ID,
            "FULL",
            schema::STATE_DDL,
        )?;
        let cache = open_file(
            &cache_path,
            "cache.db",
            schema::CACHE_APP_ID,
            "NORMAL",
            schema::CACHE_DDL,
        )?;
        Ok(Self {
            cache,
            state,
            cache_path,
        })
    }

    #[must_use]
    pub fn cache(&self) -> &Connection {
        &self.cache
    }

    #[must_use]
    pub fn state(&self) -> &Connection {
        &self.state
    }

    /// Truncate every cache.db table (children first — FKs are on).
    ///
    /// This is the first step of bare-metal recovery (D15): state.db (or
    /// the latest CAS snapshot) supplies tags + the dat-source list, then
    /// a store scan repopulates blobs/aliases/recipes and deterministic
    /// re-import rebuilds the dat model. The repopulation half lands with
    /// the ingest pipeline (M1 critical path).
    pub fn truncate_cache(&mut self) -> Result<(), IndexError> {
        let tx = self.cache.transaction()?;
        for table in schema::CACHE_TABLES_CHILD_FIRST {
            tx.execute_batch(&format!("DELETE FROM {table};"))?;
        }
        tx.commit()?;
        Ok(())
    }

    #[must_use]
    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }
}

fn open_file(
    path: &Path,
    label: &'static str,
    app_id: u32,
    synchronous: &str,
    ddl: &str,
) -> Result<Connection, IndexError> {
    let conn = Connection::open(path)?;
    // page_size must precede WAL; a WAL database's page size is frozen.
    conn.pragma_update(None, "page_size", 8192)?;
    // journal_mode returns a result row; query instead of execute.
    conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))?;
    conn.pragma_update(None, "synchronous", synchronous)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let found_app: u32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let found_version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    match (found_app, found_version) {
        (0, 0) => {
            // Fresh file: create everything atomically, then stamp it.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(ddl)?;
            tx.commit()?;
            conn.pragma_update(None, "application_id", app_id)?;
            conn.pragma_update(None, "user_version", schema::SCHEMA_VERSION)?;
        }
        (app, _) if app != app_id => {
            return Err(IndexError::WrongApplicationId {
                file: label,
                found: app,
            });
        }
        (_, version) if version != schema::SCHEMA_VERSION => {
            return Err(IndexError::SchemaVersion {
                file: label,
                found: version,
                expected: schema::SCHEMA_VERSION,
            });
        }
        _ => {}
    }
    Ok(conn)
}
