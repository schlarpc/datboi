//! Filesystem blob store (D14/D19/D20): loose hash-named files under
//! data/ and meta/ namespaces, written tmp → fsync → atomic rename.
//!
//! The API is synchronous: M1 is CLI-driven batch ingest, the store is the
//! lowest layer, and a sync core composes with any future async daemon via
//! `spawn_blocking` — whereas async-first would bake a runtime choice into
//! the one crate that must outlive everything else.
//!
//! NOTE: shard fanout in [`layout`] is a placeholder; the M1 NFS benchmark
//! freezes the real constant before the on-disk format is declared stable
//! (docs/90-roadmap.md, prototype 1).

mod crash;
pub mod layout;
pub mod obao;
pub mod store;

pub use layout::Namespace;
pub use store::{PutOutcome, Store, StoreError, VerifyOutcome};
