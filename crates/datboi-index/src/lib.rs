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

pub mod analysis;
pub mod blobs;
pub mod dats;
pub mod recipes;
pub mod schema;
pub mod state;
pub mod types;

use std::path::{Path, PathBuf};

use rusqlite::Connection;

pub use analysis::{AnalysisOutcome, SweepItem};
pub use blobs::BlobRow;
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
            schema::STATE_SCHEMA_VERSION,
            OnOlderVersion::Migrate(schema::STATE_MIGRATIONS),
        )?;
        let cache = open_file(
            &cache_path,
            "cache.db",
            schema::CACHE_APP_ID,
            "NORMAL",
            schema::CACHE_DDL,
            schema::CACHE_SCHEMA_VERSION,
            OnOlderVersion::DropAndRecreate,
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

/// What to do with a validly-ours file whose schema version is OLDER
/// than this build supports. (Newer is always a hard error: downgrades
/// are unsupported in both files.)
enum OnOlderVersion {
    /// cache.db (D37): the file is derivable, so delete it (and its WAL
    /// sidecars) and recreate empty at the current version. The caller
    /// repopulates via `datboi recover` / rescans; until then queries
    /// see an empty cache, which is a state the doctrine already
    /// requires every feature to survive.
    DropAndRecreate,
    /// state.db (D37): apply the migration ladder in place, one step
    /// per transaction, stamping `user_version` after each.
    Migrate(&'static [&'static str]),
}

fn open_file(
    path: &Path,
    label: &'static str,
    app_id: u32,
    synchronous: &str,
    ddl: &str,
    version: u32,
    on_older: OnOlderVersion,
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
            conn.pragma_update(None, "user_version", version)?;
        }
        (app, _) if app != app_id => {
            // Never touch a file that isn't ours — even the disposable
            // cache remedy must not delete a foreign database.
            return Err(IndexError::WrongApplicationId {
                file: label,
                found: app,
            });
        }
        (_, found) if found > version => {
            return Err(IndexError::SchemaVersion {
                file: label,
                found,
                expected: version,
            });
        }
        (_, found) if found < version => match on_older {
            OnOlderVersion::DropAndRecreate => {
                drop(conn);
                for suffix in ["", "-wal", "-shm"] {
                    let mut victim = path.as_os_str().to_owned();
                    victim.push(suffix);
                    match std::fs::remove_file(Path::new(&victim)) {
                        Ok(()) | Err(_) => {} // -wal/-shm may not exist
                    }
                }
                eprintln!(
                    "note: {label} was schema v{found} (this build uses v{version}); \
                     recreated empty — run `datboi recover` or re-ingest to repopulate"
                );
                return open_file(path, label, app_id, synchronous, ddl, version, on_older);
            }
            OnOlderVersion::Migrate(ladder) => {
                migrate_in_place(&conn, label, found, version, ladder)?;
            }
        },
        _ => {}
    }
    Ok(conn)
}

/// Apply `ladder[from - 1 .. to - 1]` in order, one transaction per
/// step, stamping `user_version` inside each step's transaction so a
/// crash resumes exactly where it stopped.
fn migrate_in_place(
    conn: &Connection,
    label: &'static str,
    from: u32,
    to: u32,
    ladder: &[&str],
) -> Result<(), IndexError> {
    if usize::try_from(to).unwrap_or(usize::MAX) - 1 > ladder.len() || from == 0 {
        // The ladder can't reach the target (or the file was never
        // stamped): refuse rather than guess.
        return Err(IndexError::SchemaVersion {
            file: label,
            found: from,
            expected: to,
        });
    }
    for step in from..to {
        let sql = ladder[usize::try_from(step).expect("small") - 1];
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", step + 1)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod migrate_tests {
    use super::*;

    /// The ladder machinery, exercised with a toy schema so the FIRST
    /// real state migration isn't also the first time this code runs.
    #[test]
    fn ladder_applies_steps_transactionally_and_stamps() {
        let conn = Connection::open_in_memory().expect("mem db");
        conn.execute_batch("CREATE TABLE t (v INTEGER);")
            .expect("ddl");
        conn.pragma_update(None, "user_version", 1).expect("stamp");
        let ladder: &[&str] = &[
            "ALTER TABLE t ADD COLUMN w INTEGER NOT NULL DEFAULT 7;",
            "CREATE TABLE u (x INTEGER); INSERT INTO u SELECT v FROM t;",
        ];
        conn.execute("INSERT INTO t (v) VALUES (42)", [])
            .expect("seed");
        migrate_in_place(&conn, "state.db", 1, 3, ladder).expect("migrates");
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("q");
        assert_eq!(version, 3);
        let (v, w): (i64, i64) = conn
            .query_row("SELECT v, w FROM t", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("row survived both steps");
        assert_eq!((v, w), (42, 7));
        let x: i64 = conn
            .query_row("SELECT x FROM u", [], |r| r.get(0))
            .expect("backfill ran");
        assert_eq!(x, 42);

        // Resume-from-middle: a file stamped 2 replays ONLY step 2 —
        // its schema already has column w, so accidentally re-running
        // step 1 (the ALTER) would error out; success proves the skip.
        let conn2 = Connection::open_in_memory().expect("mem db");
        conn2
            .execute_batch("CREATE TABLE t (v INTEGER, w INTEGER);")
            .expect("ddl");
        migrate_in_place(&conn2, "state.db", 2, 3, ladder).expect("migrates");
        let u_exists: i64 = conn2
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'u'",
                [],
                |r| r.get(0),
            )
            .expect("q");
        assert_eq!(u_exists, 1, "step 2 ran");

        // A ladder that can't reach the target refuses loudly.
        let err = migrate_in_place(&conn, "state.db", 3, 5, ladder).expect_err("short ladder");
        assert!(matches!(err, IndexError::SchemaVersion { .. }));
        // An unstamped (version 0) file is never guessed at.
        let err = migrate_in_place(&conn, "state.db", 0, 3, ladder).expect_err("unstamped");
        assert!(matches!(err, IndexError::SchemaVersion { .. }));
    }
}
