//! Guest-side crate for the `datboi:transform@1` world (D89,
//! docs/worlds.md §vending).
//!
//! An author's whole job:
//!
//! ```ignore
//! use datboi_guest_transform as t;
//!
//! struct Xf;
//! impl t::Guest for Xf { /* describe / run / serve-range */ }
//! t::export!(Xf);
//! ```
//!
//! or, for a transform with no reason to stream (the old whole-buffer
//! world's author experience, kept as sugar after D89 killed the ABI
//! fork):
//!
//! ```ignore
//! struct Xf;
//! impl t::BufferedGuest for Xf { /* bytes in, bytes out */ }
//! t::export_buffered!(Xf);
//! ```
//!
//! The descriptor vocabulary rides in canonical CBOR (D89): the schema
//! constants and [`Descriptor::to_cbor`] live HERE, beside the world
//! they describe; the host's decoder mirrors them
//! (datboi-runtime/src/stream.rs). Added keys must be advisory-only —
//! old hosts ignore unknown keys (D64), so anything a host must
//! understand to execute correctly is a lane version, never a new key.

// no_std + alloc so no_std guests (ex-unrar owns its panic handler and
// heap) can link this crate without dragging std's runtime in. Tests
// keep std for the harness.
#![cfg_attr(not(test), no_std)]

// Public so the exported macros (which expand in CONSUMER crates,
// possibly no_std) have a stable path to alloc.
#[doc(hidden)]
pub extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

// Bindings for the world. `pub_export_macro` + `default_bindings_module`
// are what make this crate vendable: consumers call `export!(T)` /
// `export_buffered!(T)` without ever depending on wit-bindgen
// themselves. Nested in a module because `pub_export_macro` places the
// cabi macro at the crate root — generate! at the root would collide
// with its own re-export.
// Public because the `export!` macro expands in CONSUMER crates and
// addresses types through this path; consumers should still use the
// root re-exports below.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)] // serve-range's flat 7-arg WIT shape
pub mod bindings {
    wit_bindgen::generate!({
        world: "transform",
        path: "../../wit/transform/v1",
        // The streams package is ours (D89 shared-contract lane) —
        // generate its bindings here rather than mapping to an
        // external crate.
        generate_all,
        pub_export_macro: true,
        default_bindings_module: "datboi_guest_transform::bindings",
    });
}

// The world's `use`s lift `Input` and `Sink` to the bindings root; the
// resource types live in the streams package's module. One flat root
// for consumers either way.
pub use bindings::datboi::streams::types::{File, Sink, Source};
pub use bindings::{Guest, Input, export};

pub mod cbor;

/// Descriptor CBOR keys — the schema `describe` returns
/// (docs/worlds.md; mirrored by the host decoder). `{1: seek,
/// 2: random-access-inputs}`; key 2 is omitted when empty
/// (one-encoding-per-value, the house canonical rule).
pub const DESCRIPTOR_KEY_SEEK: u64 = 1;
pub const DESCRIPTOR_KEY_RANDOM_ACCESS_INPUTS: u64 = 2;

/// Seekability class a transform declares for an op's output
/// (docs/views.md, D27). Wire values are part of the frozen schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekClass {
    /// Output ranges map to input ranges arithmetically.
    Affine,
    /// Random access via a content-addressed index (frame/block maps).
    ManifestSeekable,
    /// Whole-stream only; range reads require full materialization.
    Opaque,
}

impl SeekClass {
    #[must_use]
    pub fn wire(self) -> u64 {
        match self {
            Self::Affine => 0,
            Self::ManifestSeekable => 1,
            Self::Opaque => 2,
        }
    }
}

/// An op's static, pure capability metadata — what `describe` returns,
/// as a typed builder over the CBOR schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub seek: SeekClass,
    /// Input positions the guest will access via `file` (read-at)
    /// instead of `source` (sequential) during `run`. The host
    /// materializes non-seekable sources at these positions first
    /// (spill rule, docs/recipes.md).
    pub random_access_inputs: Vec<u32>,
}

impl Descriptor {
    #[must_use]
    pub fn opaque() -> Self {
        Self {
            seek: SeekClass::Opaque,
            random_access_inputs: Vec::new(),
        }
    }

    /// Encode as the canonical-CBOR bytes `describe` returns.
    #[must_use]
    pub fn to_cbor(&self) -> Vec<u8> {
        let mut entries = vec![(DESCRIPTOR_KEY_SEEK, cbor::Value::Uint(self.seek.wire()))];
        if !self.random_access_inputs.is_empty() {
            entries.push((
                DESCRIPTOR_KEY_RANDOM_ACCESS_INPUTS,
                cbor::Value::Array(
                    self.random_access_inputs
                        .iter()
                        .map(|&ix| cbor::Value::Uint(u64::from(ix)))
                        .collect(),
                ),
            ));
        }
        cbor::encode(&cbor::Value::Map(entries))
    }
}

/// Chunk size for the buffered adapter's reads and writes: comfortably
/// under the host's 16 MiB read ceiling, large enough that the
/// canonical-ABI copies don't dominate. Fixed, so guest-visible
/// behavior is a constant of the crate version.
pub const BUFFERED_CHUNK: u32 = 8 << 20;

/// Read a `run` input to completion, whichever shape it arrived as.
///
/// # Errors
/// If a sequential stream ends short of its declared length (the host
/// separately disproves the length claim; this is the polite twin).
pub fn read_all(input: &Input) -> Result<Vec<u8>, String> {
    match input {
        Input::Sequential(s) => {
            let len = s.len();
            let mut out = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
            while (out.len() as u64) < len {
                let want = u32::try_from((len - out.len() as u64).min(u64::from(BUFFERED_CHUNK)))
                    .expect("bounded by BUFFERED_CHUNK");
                let chunk = s.read(want);
                if chunk.is_empty() {
                    return Err(format!(
                        "input ended at {} of {len} declared bytes",
                        out.len()
                    ));
                }
                out.extend_from_slice(&chunk);
            }
            Ok(out)
        }
        Input::RandomAccess(f) => {
            let len = f.len();
            let mut out = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
            while (out.len() as u64) < len {
                let want = u32::try_from((len - out.len() as u64).min(u64::from(BUFFERED_CHUNK)))
                    .expect("bounded by BUFFERED_CHUNK");
                let chunk = f.read_at(out.len() as u64, want);
                if chunk.is_empty() {
                    return Err(format!("file ended at {} of {len} bytes", out.len()));
                }
                out.extend_from_slice(&chunk);
            }
            Ok(out)
        }
    }
}

/// Write a whole output through a sink in [`BUFFERED_CHUNK`] pieces.
pub fn write_all(sink: &Sink, bytes: &[u8]) {
    for chunk in bytes.chunks(BUFFERED_CHUNK as usize) {
        sink.write(chunk);
    }
}

/// The whole-buffer authoring surface (D89 kills the whole-buffer
/// WORLD; this is its author experience as sugar). Implement this and
/// call [`export_buffered!`]: inputs arrive complete, outputs return
/// complete, and the adapter does the streaming.
///
/// `serve-range` is answered with an error on the author's behalf — a
/// transform that can serve ranges has a reason to stream and should
/// implement [`Guest`] directly.
pub trait BufferedGuest {
    /// Static capability metadata for `op`; pure and constant. Unknown
    /// ops are errors.
    fn describe(op: &str) -> Result<Descriptor, String>;

    /// One operation: resolved input blobs in, claimed output blobs
    /// out, both in recipe order.
    fn run(op: &str, params: &[u8], inputs: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, String>;
}

/// Export a [`BufferedGuest`] as the component's `transform` world
/// implementation.
#[macro_export]
macro_rules! export_buffered {
    ($t:ty) => {
        const _: () = {
            struct DatboiBufferedAdapter;

            impl $crate::Guest for DatboiBufferedAdapter {
                fn describe(
                    op: $crate::alloc::string::String,
                ) -> Result<$crate::alloc::vec::Vec<u8>, $crate::alloc::string::String> {
                    <$t as $crate::BufferedGuest>::describe(&op).map(|d| d.to_cbor())
                }

                fn run(
                    op: $crate::alloc::string::String,
                    params: $crate::alloc::vec::Vec<u8>,
                    inputs: $crate::alloc::vec::Vec<$crate::Input>,
                    outputs: $crate::alloc::vec::Vec<$crate::Sink>,
                ) -> Result<(), $crate::alloc::string::String> {
                    let mut bufs = $crate::alloc::vec::Vec::with_capacity(inputs.len());
                    for input in &inputs {
                        bufs.push($crate::read_all(input)?);
                    }
                    let produced = <$t as $crate::BufferedGuest>::run(&op, &params, bufs)?;
                    if produced.len() != outputs.len() {
                        return Err($crate::alloc::format!(
                            "transform produced {} outputs, recipe claims {}",
                            produced.len(),
                            outputs.len()
                        ));
                    }
                    for (bytes, sink) in produced.iter().zip(&outputs) {
                        $crate::write_all(sink, bytes);
                    }
                    Ok(())
                }

                fn serve_range(
                    _op: $crate::alloc::string::String,
                    _params: $crate::alloc::vec::Vec<u8>,
                    _inputs: $crate::alloc::vec::Vec<$crate::Input>,
                    _output_ix: u32,
                    _offset: u64,
                    _len: u64,
                    _out: $crate::Sink,
                ) -> Result<(), $crate::alloc::string::String> {
                    Err("buffered transform does not serve ranges".into())
                }
            }

            $crate::export!(DatboiBufferedAdapter);
        };
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_cbor_is_canonical_and_omits_empty() {
        // {1: 2} — opaque, no random-access inputs.
        assert_eq!(Descriptor::opaque().to_cbor(), vec![0xa1, 0x01, 0x02]);
        // {1: 0, 2: [0, 3]} — affine, inputs 0 and 3 random-access.
        let d = Descriptor {
            seek: SeekClass::Affine,
            random_access_inputs: vec![0, 3],
        };
        assert_eq!(d.to_cbor(), vec![0xa2, 0x01, 0x00, 0x02, 0x82, 0x00, 0x03]);
    }
}
