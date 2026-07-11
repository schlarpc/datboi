//! Streaming reference transform for the frozen `datboi:transform@2` world:
//! the swap family, `concat`, and a read-contract probe. Its job is to
//! prove the pull-in/push-out ABI and feed the M2 determinism +
//! seek-equivalence gates (D46/D49) — production traffic uses builtins.
//!
//! The swap semantics are spec-pinned to the same clrmamepro example bytes
//! as `xf-reference` (@1) and the `swap@1` builtin. The functions are
//! duplicated rather than imported: linking the @1 crate would drag its
//! `export!` symbols into this component.
//!
//! Ops:
//! - `bitswap` / `byteswap` / `wordswap` / `wordbyteswap` — 1 sequential
//!   input, 1 output, affine. Streamed in 64 KiB chunks (a multiple of
//!   every group size, so chunking never changes the result).
//! - `concat` — N sequential inputs, 1 output, affine. The multi-input
//!   shape the interaction model was chosen for.
//! - `read-contract-probe` — copies input to output while reading in
//!   deliberately awkward sizes and *verifying the exact-read contract*
//!   (`read(n)` returns n unless EOF): the gate test for determinism
//!   layer 2.
//! - `byteswap-lying-range` — the seek-path-bug simulation D49 exists
//!   for: `run` is an honest byteswap (claims verify, replay licenses),
//!   but `serve-range` flips the first byte of every window. The output-
//!   bao check must catch it and quarantine this component's seekability
//!   (D49 rule 3); it must never surface bytes.
//!
//! Range math (`swap_range_window`, `concat_spans`) is pure and
//! host-tested; boundary off-by-ones are exactly what D49 expects to find.

/// Streaming chunk size: a multiple of every swap group size.
pub const CHUNK: usize = 64 * 1024;

/// Swap group size per op: bytes that transform together. Trailing bytes
/// that don't fill a group pass through unchanged (detector semantics).
#[must_use]
pub fn group_of(op: &str) -> Option<usize> {
    match op {
        "bitswap" => Some(1),
        "byteswap" => Some(2),
        "wordswap" | "wordbyteswap" => Some(4),
        _ => None,
    }
}

/// Apply a swap in place. `buf` MUST start on a group boundary of the
/// whole stream; a trailing partial group is left untouched.
pub fn swap_chunk(op: &str, buf: &mut [u8]) {
    match op {
        "bitswap" => {
            for b in buf {
                *b = b.reverse_bits();
            }
        }
        "byteswap" => {
            for pair in buf.chunks_exact_mut(2) {
                pair.swap(0, 1);
            }
        }
        "wordswap" => {
            for quad in buf.chunks_exact_mut(4) {
                quad.reverse();
            }
        }
        "wordbyteswap" => {
            for quad in buf.chunks_exact_mut(4) {
                quad.swap(0, 2);
                quad.swap(1, 3);
            }
        }
        _ => unreachable!("caller checked group_of"),
    }
}

/// The aligned input window whose swap contains output `[offset,
/// offset+len)`: `(window_start, window_len)`. Returns the range clamped
/// to the file, so callers slice `[offset - window_start ..][.. n]`.
#[must_use]
pub fn swap_range_window(group: u64, file_len: u64, offset: u64, len: u64) -> (u64, u64) {
    let start = offset.min(file_len);
    let end = offset.saturating_add(len).min(file_len);
    let win_start = start - start % group;
    // Round the end up to a group boundary (the whole trailing group is
    // needed to compute any byte inside it), clamped to the file.
    let win_end = end
        .checked_next_multiple_of(group)
        .unwrap_or(u64::MAX)
        .min(file_len);
    (win_start, win_end - win_start)
}

/// Which spans of which inputs make up `concat` output `[offset,
/// offset+len)`: `(input_index, input_offset, span_len)` in order.
#[must_use]
pub fn concat_spans(input_lens: &[u64], offset: u64, len: u64) -> Vec<(usize, u64, u64)> {
    let mut spans = Vec::new();
    let mut remaining = len;
    let mut pos = offset;
    let mut base = 0u64;
    for (ix, &ilen) in input_lens.iter().enumerate() {
        if remaining == 0 {
            break;
        }
        let end = base + ilen;
        if pos < end {
            let inner = pos - base;
            let take = (ilen - inner).min(remaining);
            spans.push((ix, inner, take));
            pos += take;
            remaining -= take;
        }
        base = end;
    }
    spans
}

/// Read sizes for the contract probe: deliberately awkward (odd, prime,
/// page-straddling), cycled deterministically.
pub const PROBE_SIZES: [u32; 8] = [1, 3, 7, 64, 251, 4093, 65537, 13];

/// Component glue for the frozen `datboi:transform@2` world; wasm32-only so
/// host-side tests of the pure range math build natively.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod component {
    wit_bindgen::generate!({
        world: "transform-stream",
        path: "../../wit/v2",
    });

    // `generate!` hoists types named in world signatures (Descriptor,
    // Input, Sink) to this module's root; the rest import from the
    // generated interface path.
    use datboi::transform::types::{File, SeekClass, Source};

    use super::{CHUNK, PROBE_SIZES, concat_spans, group_of, swap_chunk, swap_range_window};

    struct Xf;

    fn expect_sequential(input: &Input) -> Result<&Source, String> {
        match input {
            Input::Sequential(s) => Ok(s),
            Input::RandomAccess(_) => Err("expected a sequential input for run".into()),
        }
    }

    fn expect_random(input: &Input) -> Result<&File, String> {
        match input {
            Input::RandomAccess(f) => Ok(f),
            Input::Sequential(_) => Err("serve-range inputs must be random-access".into()),
        }
    }

    fn one_in_one_out<'a>(
        inputs: &'a [Input],
        outputs: &'a [Sink],
    ) -> Result<(&'a Source, &'a Sink), String> {
        let [input] = inputs else {
            return Err(format!("expected 1 input, got {}", inputs.len()));
        };
        let [output] = outputs else {
            return Err(format!("expected 1 output, got {}", outputs.len()));
        };
        Ok((expect_sequential(input)?, output))
    }

    impl Guest for Xf {
        fn describe(_op: String) -> Descriptor {
            // Everything here is affine and reads inputs sequentially in
            // `run`; serve-range gets random-access inputs by contract.
            Descriptor {
                seek: SeekClass::Affine,
                random_access_inputs: Vec::new(),
            }
        }

        fn run(
            op: String,
            _params: Vec<u8>,
            inputs: Vec<Input>,
            outputs: Vec<Sink>,
        ) -> Result<(), String> {
            match op.as_str() {
                _ if group_of(&op).is_some() => {
                    let (src, out) = one_in_one_out(&inputs, &outputs)?;
                    loop {
                        let mut chunk = src.read(CHUNK as u32);
                        let eof = chunk.len() < CHUNK;
                        swap_chunk(&op, &mut chunk);
                        if !chunk.is_empty() {
                            out.write(&chunk);
                        }
                        if eof {
                            return Ok(());
                        }
                    }
                }
                "concat" => {
                    let [output] = &outputs[..] else {
                        return Err(format!("expected 1 output, got {}", outputs.len()));
                    };
                    for input in &inputs {
                        let src = expect_sequential(input)?;
                        loop {
                            let chunk = src.read(CHUNK as u32);
                            let eof = chunk.len() < CHUNK;
                            if !chunk.is_empty() {
                                output.write(&chunk);
                            }
                            if eof {
                                break;
                            }
                        }
                    }
                    Ok(())
                }
                // Hostile-guest simulation: one absurd read. The HOST must
                // trap this (MAX_READ resource-abuse guard) — reaching the
                // Ok below would mean the guard is gone.
                "greedy" => {
                    let (src, _out) = one_in_one_out(&inputs, &outputs)?;
                    let n = src.read(u32::MAX).len();
                    Err(format!("host failed to trap a {n}-byte greedy read"))
                }
                // Honest byteswap; the lie lives in serve-range.
                "byteswap-lying-range" => {
                    let (src, out) = one_in_one_out(&inputs, &outputs)?;
                    loop {
                        let mut chunk = src.read(CHUNK as u32);
                        let eof = chunk.len() < CHUNK;
                        swap_chunk("byteswap", &mut chunk);
                        if !chunk.is_empty() {
                            out.write(&chunk);
                        }
                        if eof {
                            return Ok(());
                        }
                    }
                }
                "read-contract-probe" => {
                    let (src, out) = one_in_one_out(&inputs, &outputs)?;
                    let total = src.len();
                    let mut seen: u64 = 0;
                    for n in PROBE_SIZES.iter().cycle() {
                        let chunk = src.read(*n);
                        seen += chunk.len() as u64;
                        let expected = u64::from(*n).min(total - (seen - chunk.len() as u64));
                        if (chunk.len() as u64) != expected {
                            return Err(format!(
                                "read contract violated: asked {n}, got {} with {} of {total} consumed",
                                chunk.len(),
                                seen
                            ));
                        }
                        if !chunk.is_empty() {
                            out.write(&chunk);
                        }
                        if seen == total {
                            // One more read must yield exactly nothing.
                            let tail = src.read(64);
                            if !tail.is_empty() {
                                return Err("read past declared len returned bytes".into());
                            }
                            return Ok(());
                        }
                    }
                    unreachable!("cycle() never ends");
                }
                other => Err(format!("unknown op {other:?}")),
            }
        }

        fn serve_range(
            op: String,
            _params: Vec<u8>,
            inputs: Vec<Input>,
            output_ix: u32,
            offset: u64,
            len: u64,
            out: Sink,
        ) -> Result<(), String> {
            if output_ix != 0 {
                return Err(format!(
                    "no output {output_ix}: everything here is 1-output"
                ));
            }
            // The planted D49 seek bug rides the byteswap range logic.
            let lie = op == "byteswap-lying-range";
            let op = if lie { "byteswap".to_string() } else { op };
            let mut first_write = lie;
            if let Some(group) = group_of(&op) {
                let [input] = &inputs[..] else {
                    return Err(format!("expected 1 input, got {}", inputs.len()));
                };
                let file = expect_random(input)?;
                let file_len = file.len();
                let (win_start, win_len) = swap_range_window(group as u64, file_len, offset, len);
                // The gate streams ranges too: read the window in CHUNK
                // pieces (group-aligned) rather than one huge buffer.
                let mut produced: u64 = 0;
                let want = offset
                    .saturating_add(len)
                    .min(file_len)
                    .saturating_sub(offset.min(file_len));
                let mut win_pos = 0u64;
                while win_pos < win_len {
                    let take = (win_len - win_pos).min(CHUNK as u64);
                    let mut chunk = file.read_at(win_start + win_pos, take as u32);
                    if chunk.len() as u64 != take {
                        return Err("read-at contract violated".into());
                    }
                    swap_chunk(&op, &mut chunk);
                    // Slice this chunk down to the requested output range.
                    let chunk_start = win_start + win_pos;
                    let out_start = offset.min(file_len);
                    let lo = out_start.saturating_sub(chunk_start).min(take);
                    let hi = (out_start + want).saturating_sub(chunk_start).min(take);
                    if hi > lo {
                        if first_write {
                            chunk[lo as usize] ^= 0x01;
                            first_write = false;
                        }
                        out.write(&chunk[lo as usize..hi as usize]);
                        produced += hi - lo;
                    }
                    win_pos += take;
                }
                debug_assert!(produced <= want);
                Ok(())
            } else if op == "concat" {
                let files: Vec<&File> =
                    inputs.iter().map(expect_random).collect::<Result<_, _>>()?;
                let lens: Vec<u64> = files.iter().map(|f| f.len()).collect();
                for (ix, inner, span) in concat_spans(&lens, offset, len) {
                    let mut pos = 0u64;
                    while pos < span {
                        let take = (span - pos).min(CHUNK as u64);
                        let chunk = files[ix].read_at(inner + pos, take as u32);
                        if chunk.len() as u64 != take {
                            return Err("read-at contract violated".into());
                        }
                        out.write(&chunk);
                        pos += take;
                    }
                }
                Ok(())
            } else {
                Err(format!("op {op:?} does not serve ranges"))
            }
        }
    }

    export!(Xf);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same spec pins as xf-reference (@1): these two crates must never
    /// disagree about swap semantics.
    #[test]
    fn swaps_match_clrmamepro_spec() {
        let mut ws = vec![0x01, 0x02, 0x03, 0x04];
        swap_chunk("wordswap", &mut ws);
        assert_eq!(ws, [0x04, 0x03, 0x02, 0x01]);

        let mut wbs = vec![0x01, 0x02, 0x03, 0x04];
        swap_chunk("wordbyteswap", &mut wbs);
        assert_eq!(wbs, [0x03, 0x04, 0x01, 0x02]);

        let mut bs = vec![0x80, 0x37, 0x12, 0x40, 0xff];
        swap_chunk("byteswap", &mut bs);
        assert_eq!(bs, [0x37, 0x80, 0x40, 0x12, 0xff]);
    }

    #[test]
    fn chunking_never_changes_swaps() {
        // CHUNK is a multiple of every group: applying per-chunk equals
        // applying whole-buffer.
        let data: Vec<u8> = (0..(CHUNK * 2 + 5)).map(|i| (i % 251) as u8).collect();
        for op in ["bitswap", "byteswap", "wordswap", "wordbyteswap"] {
            let mut whole = data.clone();
            swap_chunk(op, &mut whole);
            let mut chunked = data.clone();
            for piece in chunked.chunks_mut(CHUNK) {
                swap_chunk(op, piece);
            }
            assert_eq!(whole, chunked, "{op}");
        }
    }

    #[test]
    fn swap_windows_cover_and_align() {
        for group in [1u64, 2, 4] {
            for file_len in [0u64, 1, 3, 4, 5, 100, 101, 102, 103] {
                for offset in 0..=file_len + 2 {
                    for len in [0u64, 1, 2, 3, 7, 100] {
                        let (start, wlen) = swap_range_window(group, file_len, offset, len);
                        assert_eq!(start % group, 0, "window starts on a group boundary");
                        assert!(start + wlen <= file_len, "window inside the file");
                        // Window covers the clamped request.
                        let req_start = offset.min(file_len);
                        let req_end = (offset + len).min(file_len);
                        assert!(start <= req_start);
                        assert!(start + wlen >= req_end);
                    }
                }
            }
        }
    }

    #[test]
    fn concat_spans_partition_the_range() {
        let lens = [5u64, 0, 7, 3];
        // Whole output.
        let spans = concat_spans(&lens, 0, 15);
        assert_eq!(spans, vec![(0, 0, 5), (2, 0, 7), (3, 0, 3)]);
        // Straddling the 5-boundary and the 12-boundary.
        assert_eq!(concat_spans(&lens, 4, 2), vec![(0, 4, 1), (2, 0, 1)]);
        assert_eq!(concat_spans(&lens, 11, 3), vec![(2, 6, 1), (3, 0, 2)]);
        // Past the end.
        assert_eq!(concat_spans(&lens, 15, 4), vec![]);
        assert_eq!(concat_spans(&lens, 14, 4), vec![(3, 2, 1)]);
    }
}
