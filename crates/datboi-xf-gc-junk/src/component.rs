//! `xf-gc-junk`: the `fill` op for the `datboi:transform@1` world.
//!
//! One op, zero inputs, one output: `fill {id, disc, len}` regenerates
//! the mastering junk of a GameCube disc (or a Wii partition) over the
//! whole address space `[0, len)` (D115). A disc's rebuild recipe is a
//! builtin assemble whose junk segments are ranges of this stream at
//! the SAME offsets the junk occupies on the disc; the component knows
//! nothing about discs, only the generator.
//!
//! `serve-range` is positional: the generator reseeds from the sector
//! index and skips into the sector, so a window costs the bytes it
//! covers and nothing before them.

use datboi_guest_transform::alloc::{format, string::String, vec, vec::Vec};
use datboi_guest_transform::{Descriptor, Guest, Input, SeekClass, Sink};

use crate::params::Params;
use crate::{Lfg, fill_at};

/// Bytes generated per sink write (128 KiB): under the host's write
/// ceiling, large enough that canonical-ABI copies don't dominate.
const BYTES_PER_WRITE: u64 = 128 * 1024;

const OP: &str = "fill";

struct Xf;

fn params(op: &str, params: &[u8]) -> Result<Params, String> {
    if op != OP {
        return Err(format!("unknown op {op:?}"));
    }
    Params::decode(params).map_err(String::from)
}

fn write_range(p: &Params, start: u64, end: u64, out: &Sink) {
    let mut lfg = Lfg::default();
    let mut buf = vec![0u8; usize::try_from(BYTES_PER_WRITE).expect("small")];
    let mut pos = start;
    while pos < end {
        let n = usize::try_from((end - pos).min(BYTES_PER_WRITE)).expect("bounded");
        fill_at(&mut lfg, p.id, p.disc, pos, &mut buf[..n]);
        out.write(&buf[..n]);
        pos += n as u64;
    }
}

impl Guest for Xf {
    fn describe(op: String) -> Result<Vec<u8>, String> {
        if op != OP {
            return Err(format!("unknown op {op:?}"));
        }
        // No manifest and no inputs: a byte offset IS a stream
        // position, reached by reseeding the sector it lies in.
        Ok(Descriptor {
            seek: SeekClass::Affine,
            random_access_inputs: Vec::new(),
        }
        .to_cbor())
    }

    fn run(
        op: String,
        params_bytes: Vec<u8>,
        inputs: Vec<Input>,
        outputs: Vec<Sink>,
    ) -> Result<(), String> {
        let p = params(&op, &params_bytes)?;
        if !inputs.is_empty() {
            return Err(format!("fill takes no inputs, got {}", inputs.len()));
        }
        let [output] = &outputs[..] else {
            return Err(format!("expected 1 output, got {}", outputs.len()));
        };
        if p.len == 0 {
            return Err("fill of zero bytes has no output (the empty blob needs no recipe)".into());
        }
        write_range(&p, 0, p.len, output);
        Ok(())
    }

    fn serve_range(
        op: String,
        params_bytes: Vec<u8>,
        inputs: Vec<Input>,
        output_ix: u32,
        offset: u64,
        len: u64,
        out: Sink,
    ) -> Result<(), String> {
        let p = params(&op, &params_bytes)?;
        if !inputs.is_empty() {
            return Err(format!("fill takes no inputs, got {}", inputs.len()));
        }
        if output_ix != 0 {
            return Err(format!("no output {output_ix}: fill is 1-output"));
        }
        let start = offset.min(p.len);
        let end = offset.saturating_add(len).min(p.len);
        if end > start {
            write_range(&p, start, end, &out);
        }
        Ok(())
    }
}

datboi_guest_transform::export!(Xf);
