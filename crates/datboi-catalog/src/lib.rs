//! Dat import, content-identity unification, audit rollups, dir2dat.
//!
//! Design record: docs/60-dats.md, docs/65-schema.md §2–§3, decisions
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
pub mod diff;
pub mod export;
pub mod import;
pub mod rollup;
pub mod unify;

pub use audit::{AuditReport, EntryAudit, audit};
pub use diff::{DatDiff, diff_source};
pub use export::export_dat;
pub use import::{ImportOptions, ImportReport, import_dat};
pub use rollup::refresh_rollups;
pub use unify::relink_all;

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
    #[error("export: {0}")]
    Export(String),
    #[error("unknown dat source {provider}/{system}")]
    UnknownSource { provider: String, system: String },
    #[error("source {provider}/{system} has no current revision")]
    NoCurrentRevision { provider: String, system: String },
    #[error(
        "source {provider}/{system} has only one materialized revision — import a newer dat first"
    )]
    NoPreviousRevision { provider: String, system: String },
}
