//! CLI environment resolution: flags > `DATBOI_*` env (12-factor,
//! docs/50-infra.md).
//!
//! The store root and DB dir are both REQUIRED and deliberately have no
//! default relative to each other: the store may live on NFS while
//! embedded databases must stay on daemon-local disk (D15/D37).
//! Ergonomic defaults can come later; wrong defaults here risk a redb-on-
//! NFS-class mistake.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Args;
use datboi_formats::skipper::Detector;
use datboi_index::Db;
use datboi_store_fs::Store;

#[derive(Args, Debug)]
pub struct GlobalArgs {
    /// Store root directory (data/, meta/, tmp/) — may be on NFS.
    #[arg(long, env = "DATBOI_STORE", global = true, value_name = "DIR")]
    pub store: Option<PathBuf>,

    /// Database directory (cache.db, state.db) — MUST be local disk,
    /// never NFS (D15).
    #[arg(long, env = "DATBOI_DB_DIR", global = true, value_name = "DIR")]
    pub db_dir: Option<PathBuf>,

    /// Directory of header-skipper detector XMLs (optional).
    #[arg(long, env = "DATBOI_DETECTORS", global = true, value_name = "DIR")]
    pub detectors: Option<PathBuf>,
}

/// Everything a storage-touching command needs, opened.
pub struct Env {
    pub store: Store,
    pub db: Db,
    pub detectors: Vec<Detector>,
    /// Non-fatal detector load problems, surfaced once per run.
    pub detector_errors: Vec<(PathBuf, String)>,
}

impl GlobalArgs {
    pub fn open(&self) -> anyhow::Result<Env> {
        let Some(store_root) = &self.store else {
            bail!("store root not set: pass --store or set DATBOI_STORE");
        };
        let Some(db_dir) = &self.db_dir else {
            bail!("database dir not set: pass --db-dir or set DATBOI_DB_DIR (local disk, not NFS)");
        };
        let store = Store::open(store_root)
            .with_context(|| format!("opening store at {}", store_root.display()))?;
        std::fs::create_dir_all(db_dir)
            .with_context(|| format!("creating db dir {}", db_dir.display()))?;
        let db = Db::open(db_dir)
            .with_context(|| format!("opening databases in {}", db_dir.display()))?;
        let (detectors, detector_errors) = match &self.detectors {
            Some(dir) => datboi_ingest::load_detectors(dir),
            None => (Vec::new(), Vec::new()),
        };
        Ok(Env {
            store,
            db,
            detectors,
            detector_errors,
        })
    }
}
