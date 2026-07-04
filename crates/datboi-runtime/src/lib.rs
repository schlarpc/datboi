//! wasmtime host for content-addressed transforms.
//!
//! Design record: docs/30-runtime.md, decisions D5–D7. The wasmtime
//! dependency lands with M1 prototype 3 (determinism PoC); until then this
//! crate only carries the shared vocabulary.

/// Seekability classes declared by transforms (docs/80-views.md, D27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekClass {
    /// Output ranges map to input ranges arithmetically.
    Affine,
    /// Random access via a content-addressed index (frame/block tables).
    ManifestSeekable,
    /// Whole-stream only; range reads require materialization first.
    Opaque,
}
