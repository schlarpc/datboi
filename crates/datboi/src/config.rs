//! CLI environment resolution: flags > `DATBOI_*` env (12-factor,
//! docs/50-infra.md).
//!
//! The store root is REQUIRED and has no default: it may live on NFS, so
//! a wrong guess risks a redb-on-NFS-class mistake (D15/D37). The DB dir
//! defaults to `$XDG_STATE_HOME/datboi` (fallback `~/.local/state/datboi`)
//! — that's daemon-local disk by construction, which is exactly the
//! placement D15 requires. The container image overrides it with an
//! explicit local volume (docs/50-infra.md).

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
    /// never NFS (D15). Defaults to `$XDG_STATE_HOME/datboi`
    /// (`~/.local/state/datboi`).
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
/// Identity helpers moved to datboi-catalog::statesnap (D75: the
/// daemon's auto-cadence needs them too); these delegates keep the
/// CLI's call sites and anyhow error shape.
pub fn load_identity(db_dir: &std::path::Path) -> anyhow::Result<Option<Identity>> {
    Ok(datboi_catalog::statesnap::load_identity(db_dir)?)
}

/// Load the identity, generating and persisting one on first use (0600).
pub fn load_or_create_identity(db_dir: &std::path::Path) -> anyhow::Result<Identity> {
    Ok(datboi_catalog::statesnap::load_or_create_identity(db_dir)?)
}

/// The default DB dir per the XDG Base Directory spec: `$XDG_STATE_HOME`
/// (relative values ignored per spec), falling back to `~/.local/state`.
/// The DBs (and the D15 identity key) are local per-instance state that
/// must persist across restarts — state, not config or cache. `None` only
/// when neither var yields an absolute base (headless with no `$HOME`).
fn default_db_dir() -> Option<PathBuf> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        let state = PathBuf::from(state);
        if state.is_absolute() {
            return Some(state.join("datboi"));
        }
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if home.as_os_str().is_empty() {
        return None;
    }
    Some(home.join(".local/state/datboi"))
}

impl GlobalArgs {
    /// The resolved DB dir: `--db-dir`/`DATBOI_DB_DIR` if set, else the
    /// XDG default. `Db::open` creates it (D15 owns its preconditions).
    pub fn resolve_db_dir(&self) -> anyhow::Result<PathBuf> {
        if let Some(dir) = &self.db_dir {
            return Ok(dir.clone());
        }
        default_db_dir().ok_or_else(|| {
            anyhow::anyhow!(
                "database dir not set and no $XDG_STATE_HOME or $HOME to derive a \
                 default: pass --db-dir or set DATBOI_DB_DIR (local disk, not NFS)"
            )
        })
    }

    pub fn open(&self) -> anyhow::Result<Env> {
        let Some(store_root) = &self.store else {
            bail!("store root not set: pass --store or set DATBOI_STORE");
        };
        let db_dir = self.resolve_db_dir()?;
        let store = Store::open(store_root)
            .with_context(|| format!("opening store at {}", store_root.display()))?;
        let db = Db::open(&db_dir)
            .with_context(|| format!("opening databases in {}", db_dir.display()))?;
        let (detectors, detector_errors) = match &self.detectors {
            Some(dir) => datboi_ingest::load_detectors(dir),
            None => (Vec::new(), Vec::new()),
        };
        Ok(Env {
            store,
            db,
            db_dir,
            detectors,
            detector_errors,
        })
    }
}
