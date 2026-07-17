//! Dat import, content-identity unification, audit rollups, dir2dat.
//!
//! Design record: docs/dats.md, docs/schema.md §2–§3, decisions
//! D2/D29/D38/D39. Import is a deterministic function of the dat blob
//! (D15); the only wall-clock in any row is `imported_at`.
//!
//! ## have(verified) vs have(claimed) — the M1 interpretation
//!
//! Zip members ingested locally are hashed from real bytes during ingest;
//! their derive recipes are marked `Verified` with `source = LocalIngest`
//! (D4: verify on ingest, trust after). For *audit* purposes those blobs
//! count as **have(verified)** — we computed the hashes ourselves — even
//! though their literal bytes are absent. `have(claimed)` is reserved for
//! verified-grade claims from other sources (peers, M6+). Neither audit
//! notion licenses dropping literals: eviction remains gated on
//! `ReplayedLocal` (D25), untouched.

pub mod audit;
pub mod clonelist;
pub mod diff;
pub mod export;
pub mod fat32;
pub mod fetch;
pub mod image;
pub mod import;
pub mod mame;
pub mod profiles;
pub mod rollup;
pub mod selection;
pub mod state;
pub mod statesnap;
pub mod unify;
pub mod views;

pub use audit::{AuditReport, EntryAudit, audit};
pub use clonelist::{ClonelistReport, import_clonelist, load_clonelist};
pub use diff::{DatDiff, diff_source};
pub use export::export_dat;
pub use fetch::{FetchedDat, fetch_dat};
pub use image::{ImageParams, ImageReport, mint_image, missing_inputs};
pub use import::{ImportOptions, ImportReport, import_dat};
pub use mame::MameMode;
pub use profiles::{PROFILES, Profile};
pub use rollup::refresh_rollups;
pub use selection::SelectionPolicy;
pub use state::{RollupState, STATE_CASE_SQL};
pub use unify::relink_all;
pub use views::{EvalReport, ViewDef, define_view, evaluate_view, get_view, list_views};

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    Store(#[from] datboi_store_fs::StoreError),
    #[error(transparent)]
    Index(#[from] datboi_index::IndexError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Parse(#[from] datboi_formats::model::ParseError),
    #[error(transparent)]
    Xml(#[from] quick_xml::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("corrupt {0} record")]
    Corrupt(&'static str),
    #[error(transparent)]
    Fat32(#[from] fat32::Fat32Error),
    #[error("image mint: {0}")]
    Image(String),
    #[error("clonelist: {0}")]
    Clonelist(String),
    #[error(transparent)]
    Snapshot(#[from] datboi_core::snapshot::SnapshotError),
    #[error("snapshot: {0}")]
    Statesnap(String),
    #[error("mame mode: {0}")]
    Mame(String),
    #[error("export: {0}")]
    Export(String),
    #[error("dat fetch: {0}")]
    Fetch(String),
    #[error("unknown profile {0} (see `datboi view profiles`)")]
    UnknownProfile(String),
    #[error("unknown dat source {provider}/{system}")]
    UnknownSource { provider: String, system: String },
    #[error("source {provider}/{system} has no current revision")]
    NoCurrentRevision { provider: String, system: String },
    #[error(
        "source {provider}/{system} has only one materialized revision — import a newer dat first"
    )]
    NoPreviousRevision { provider: String, system: String },
}
