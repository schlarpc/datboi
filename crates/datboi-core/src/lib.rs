//! Core domain model: content addressing, structured-object typing, recipes.
//!
//! Design record: docs/10-cas.md, docs/70-recipes.md, decisions D2/D18/D20.

pub mod alias;
pub mod assemble;
pub mod cbor;
pub mod hash;
pub mod identity;
pub mod object;
pub mod params;
pub mod recipe;
pub mod snapshot;
pub mod viewsnap;
