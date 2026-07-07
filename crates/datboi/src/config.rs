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
use datboi_core::identity::Identity;
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
    /// Where the DBs (and the identity key file) live.
    pub db_dir: PathBuf,
    pub detectors: Vec<Detector>,
    /// Non-fatal detector load problems, surfaced once per run.
    pub detector_errors: Vec<(PathBuf, String)>,
}

/// The instance identity key file: 32 raw seed bytes next to state.db.
/// The one non-CAS secret (D15) — recovery needs it to authenticate the
/// snapshot it trusts, so it must survive DB nukes and be backed up
/// out-of-band.
fn identity_path(db_dir: &std::path::Path) -> PathBuf {
    db_dir.join("identity.key")
}

/// Load the identity if its key file exists; `None` means "never created".
pub fn load_identity(db_dir: &std::path::Path) -> anyhow::Result<Option<Identity>> {
    let path = identity_path(db_dir);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let seed: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("{} must be exactly 32 bytes", path.display()))?;
            Ok(Some(Identity::from_seed(seed)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Load the identity, generating and persisting one on first use (0600).
pub fn load_or_create_identity(db_dir: &std::path::Path) -> anyhow::Result<Identity> {
    if let Some(identity) = load_identity(db_dir)? {
        return Ok(identity);
    }
    let identity = Identity::generate().context("generating instance identity")?;
    let path = identity_path(db_dir);
    // Owner-only: this seed IS the instance identity.
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        file.write_all(&identity.to_seed())?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, identity.to_seed())
        .with_context(|| format!("creating {}", path.display()))?;
    eprintln!(
        "note: generated instance identity at {} — back this file up out-of-band (D15)",
        path.display()
    );
    Ok(identity)
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
            db_dir: db_dir.clone(),
            detectors,
            detector_errors,
        })
    }
}
