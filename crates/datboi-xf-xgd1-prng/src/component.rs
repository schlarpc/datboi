//! `xf-xgd1-prng`: the `fill` op for the `datboi:transform@1` world.
//!
//! One op, zero inputs, one output: `fill {seed, sectors}` regenerates
//! the mastering filler stream of an XGD1 disc — `sectors` × 2048 bytes
//! from the 32-bit `seed` (D111). The disc's rebuild recipe is a
//! builtin assemble whose filler segments are ranges of this stream;
//! the component knows nothing about discs, only the generator.
//!
//! `serve-range` is arithmetic: the generator jumps to any stream sector
//! by one modular exponentiation ([`Prng::new`]), so a window costs the
//! sectors it touches and nothing before them.

use datboi_guest_transform::alloc::{format, string::String, vec::Vec};
use datboi_guest_transform::{Descriptor, Guest, Input, SeekClass, Sink};

use crate::params::Params;
use crate::{Prng, SECTOR_LEN};

/// Sectors generated per sink write (128 KiB): well under the host's
/// read/write ceilings, large enough that canonical-ABI copies don't
/// dominate the ~1k field multiplications per sector.
const SECTORS_PER_WRITE: u64 = 64;

const OP: &str = "fill";

struct Xf;

fn params(op: &str, params: &[u8]) -> Result<Params, String> {
    if op != OP {
        return Err(format!("unknown op {op:?}"));
    }
    Params::decode(params).map_err(String::from)
}

/// Write stream sectors `[first, first + count)` to `out`, trimming
/// `skip_head` bytes off the first sector and `keep_tail` bytes into
/// the last (both may be zero; a single-sector window applies both).
fn write_sectors(
    seed: u32,
    first: u64,
    count: u64,
    skip_head: usize,
    keep_tail: usize,
    out: &Sink,
) {
    let mut prng = Prng::new(
        seed,
        u32::try_from(first).expect("sector index bounded by params"),
    );
    let mut buf =
        Vec::with_capacity(SECTOR_LEN * usize::try_from(SECTORS_PER_WRITE).expect("small"));
    let mut sector = [0u8; SECTOR_LEN];
    let mut remaining = count;
    let mut index = 0u64;
    while remaining > 0 {
        buf.clear();
        let batch = remaining.min(SECTORS_PER_WRITE);
        for _ in 0..batch {
            prng.fill_sector(&mut sector);
            let a = if index == 0 { skip_head } else { 0 };
            let b = if index == count - 1 {
                keep_tail
            } else {
                SECTOR_LEN
            };
            buf.extend_from_slice(&sector[a..b]);
            index += 1;
        }
        out.write(&buf);
        remaining -= batch;
    }
}

impl Guest for Xf {
    fn describe(op: String) -> Result<Vec<u8>, String> {
        if op != OP {
            return Err(format!("unknown op {op:?}"));
        }
        // No manifest and no inputs: a byte offset IS a stream position,
        // reached by one modular exponentiation.
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
        if p.sectors == 0 {
            return Err(
                "fill of zero sectors has no output (the empty blob needs no recipe)".into(),
            );
        }
        write_sectors(p.seed, 0, p.sectors, 0, SECTOR_LEN, output);
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
        let total = p.sectors * SECTOR_LEN as u64;
        let start = offset.min(total);
        let end = offset.saturating_add(len).min(total);
        if end <= start {
            return Ok(());
        }
        let first = start / SECTOR_LEN as u64;
        let last = (end - 1) / SECTOR_LEN as u64;
        let skip_head = usize::try_from(start - first * SECTOR_LEN as u64).expect("< sector");
        let keep_tail = usize::try_from(end - last * SECTOR_LEN as u64).expect("<= sector");
        write_sectors(p.seed, first, last - first + 1, skip_head, keep_tail, &out);
        Ok(())
    }
}

datboi_guest_transform::export!(Xf);
