//! Filesystem blob store (D14/D19/D20): loose hash-named files under
//! data/ and meta/ namespaces, written tmp → fsync → atomic rename.
//!
//! NOTE: shard fanout below is a placeholder; the M1 NFS benchmark freezes
//! the real constant before the on-disk format is declared stable
//! (docs/90-roadmap.md, prototype 1).

pub mod layout;
