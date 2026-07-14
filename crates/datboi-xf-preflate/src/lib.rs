//! `xf-preflate`: deflate-stream recreation for the frozen
//! `datboi:transform@2` world (D53).
//!
//! One op, `recreate`: rebuild a member's raw deflate bytes bit-exactly
//! from two sequential inputs —
//!
//! * `inputs[0]` — the **corrections blob** (recipe role `skeleton`): the
//!   framed output of the native preflate split (see below);
//! * `inputs[1]` — the **member plaintext**, the ordinary CAS blob the
//!   member decompresses to.
//!
//! Output `[0]` is the raw deflate stream (no zip headers — the container
//! is an `assemble@1` over literal structure segments + rebuilt streams).
//! Seek class is `opaque`: deflate bit-packing has no cheap range math,
//! so `serve-range` refuses and range reads of the *container* ride
//! assemble's affine math instead, materializing only touched members.
//!
//! ## Corrections framing (v1, owned by this component's hash)
//!
//! The split side (native analyzer) walks the deflate stream in bounded
//! windows via `PreflateStreamProcessor`, emitting one frame per
//! `decompress` call:
//!
//! ```text
//! frame := plaintext_len: u32 LE | corrections_len: u32 LE
//!        | corrections bytes
//! blob  := frame*            (end of blob terminates)
//! ```
//!
//! Frame plaintext lengths partition the plaintext input exactly, so the
//! plaintext blob stays a plain member plaintext — all framing lives in
//! the corrections blob. Rebuild memory is bounded by the largest frame
//! (splitter caps plaintext at 32 MiB per frame; we defensively accept up
//! to [`MAX_FRAME`]) plus preflate's 32 KiB carry-over dictionary.
//! `RecreateStreamProcessor` keeps deflate bit state across frames, so
//! concatenated frame outputs are the original stream bit-for-bit.
//!
//! The recipe's D4 output-hash check makes all of this fail-safe: a
//! corrupt corrections blob wastes CPU but can never surface wrong bytes.

/// Defensive per-frame ceiling (bytes) for both plaintext and corrections
/// lengths. The splitter stays well under this; anything larger is a
/// malformed blob and errors deterministically.
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

/// Streaming read size: comfortably under the host's 16 MiB MAX_READ trap.
pub const CHUNK: u32 = 8 * 1024 * 1024;

/// One parsed frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub plaintext_len: u32,
    pub corrections_len: u32,
}

/// Parse a frame header. `None` for an empty slice (end of blob);
/// `Err` for a truncated or oversized header.
///
/// # Errors
/// If the header is truncated (1–7 bytes) or either length exceeds
/// [`MAX_FRAME`].
pub fn parse_frame_header(bytes: &[u8]) -> Result<Option<FrameHeader>, String> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let Ok(fixed): Result<&[u8; 8], _> = bytes.try_into() else {
        return Err(format!(
            "truncated corrections frame header: {} of 8 bytes",
            bytes.len()
        ));
    };
    let plaintext_len = u32::from_le_bytes(fixed[..4].try_into().expect("4 bytes"));
    let corrections_len = u32::from_le_bytes(fixed[4..].try_into().expect("4 bytes"));
    if plaintext_len > MAX_FRAME || corrections_len > MAX_FRAME {
        return Err(format!(
            "corrections frame exceeds MAX_FRAME ({MAX_FRAME}): plaintext={plaintext_len} corrections={corrections_len}"
        ));
    }
    Ok(Some(FrameHeader {
        plaintext_len,
        corrections_len,
    }))
}

/// Encode a frame header (the split side's counterpart, used natively by
/// the analyzer and by tests).
#[must_use]
pub fn encode_frame_header(h: FrameHeader) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&h.plaintext_len.to_le_bytes());
    out[4..].copy_from_slice(&h.corrections_len.to_le_bytes());
    out
}

/// Component glue for the frozen `datboi:transform@2` world; wasm32-only
/// so host-side tests of the pure framing build natively.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod component {
    wit_bindgen::generate!({
        world: "transform-stream",
        path: "../../wit/transform/v2",
    });

    use std::io::Cursor;

    use datboi::transform::types::{SeekClass, Source};
    use preflate_rs::RecreateStreamProcessor;

    use super::{CHUNK, parse_frame_header};

    struct Xf;

    fn expect_sequential(input: &Input) -> Result<&Source, String> {
        match input {
            Input::Sequential(s) => Ok(s),
            Input::RandomAccess(_) => Err("recreate reads all inputs sequentially".into()),
        }
    }

    /// Read exactly `n` bytes in MAX_READ-safe pieces. The exact-read
    /// contract means any shortfall is true end-of-stream — malformed
    /// input, reported as such.
    fn read_exactly(src: &Source, n: u32, what: &str) -> Result<Vec<u8>, String> {
        let mut buf = Vec::with_capacity(n as usize);
        let mut remaining = n;
        while remaining > 0 {
            let take = remaining.min(CHUNK);
            let piece = src.read(take);
            let got = u32::try_from(piece.len()).expect("read bounded by take");
            buf.extend_from_slice(&piece);
            if got < take {
                return Err(format!(
                    "{what} ended early: wanted {n} bytes, stream ended after {}",
                    buf.len()
                ));
            }
            remaining -= take;
        }
        Ok(buf)
    }

    impl Guest for Xf {
        fn describe(_op: String) -> Descriptor {
            Descriptor {
                seek: SeekClass::Opaque,
                random_access_inputs: Vec::new(),
            }
        }

        fn run(
            op: String,
            params: Vec<u8>,
            inputs: Vec<Input>,
            outputs: Vec<Sink>,
        ) -> Result<(), String> {
            if op != "recreate" {
                return Err(format!("unknown op {op:?}"));
            }
            if !params.is_empty() {
                return Err("recreate takes no params: framing lives in the corrections blob".into());
            }
            let [corrections_in, plaintext_in] = &inputs[..] else {
                return Err(format!(
                    "expected 2 inputs (corrections, plaintext), got {}",
                    inputs.len()
                ));
            };
            let [output] = &outputs[..] else {
                return Err(format!("expected 1 output, got {}", outputs.len()));
            };
            let corrections_in = expect_sequential(corrections_in)?;
            let plaintext_in = expect_sequential(plaintext_in)?;

            let mut recreate = RecreateStreamProcessor::new();
            let mut plaintext_consumed: u64 = 0;
            loop {
                let header_bytes = corrections_in.read(8);
                let Some(header) = parse_frame_header(&header_bytes)? else {
                    break;
                };
                let corrections =
                    read_exactly(corrections_in, header.corrections_len, "corrections frame")?;
                let plaintext =
                    read_exactly(plaintext_in, header.plaintext_len, "plaintext input")?;
                plaintext_consumed += u64::from(header.plaintext_len);
                let (bytes, _blocks) = recreate
                    .recompress(&mut Cursor::new(&plaintext), &corrections)
                    .map_err(|e| format!("preflate recreate failed: {e}"))?;
                if !bytes.is_empty() {
                    output.write(&bytes);
                }
            }
            // Frames must partition the plaintext exactly.
            let declared = plaintext_in.len();
            if plaintext_consumed != declared {
                return Err(format!(
                    "corrections frames cover {plaintext_consumed} of {declared} plaintext bytes"
                ));
            }
            Ok(())
        }

        fn serve_range(
            op: String,
            _params: Vec<u8>,
            _inputs: Vec<Input>,
            _output_ix: u32,
            _offset: u64,
            _len: u64,
            _out: Sink,
        ) -> Result<(), String> {
            Err(format!("op {op:?} is opaque: ranges require materialization"))
        }
    }

    export!(Xf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_roundtrip() {
        let h = FrameHeader {
            plaintext_len: 32 * 1024 * 1024,
            corrections_len: 517,
        };
        let bytes = encode_frame_header(h);
        assert_eq!(parse_frame_header(&bytes), Ok(Some(h)));
    }

    #[test]
    fn empty_is_end_of_blob() {
        assert_eq!(parse_frame_header(&[]), Ok(None));
    }

    #[test]
    fn truncated_header_is_malformed() {
        assert!(parse_frame_header(&[1, 2, 3]).is_err());
    }

    #[test]
    fn oversized_frame_is_malformed() {
        let bytes = encode_frame_header(FrameHeader {
            plaintext_len: MAX_FRAME + 1,
            corrections_len: 0,
        });
        assert!(parse_frame_header(&bytes).is_err());
    }

    /// The whole point: frames produced by the native split
    /// (`PreflateStreamProcessor`) recreate bit-exactly through the same
    /// `RecreateStreamProcessor` the guest uses — including deflate bit
    /// state carried across frame boundaries.
    #[test]
    fn framed_split_recreates_bit_exactly() {
        use std::io::Cursor;

        use preflate_rs::{
            ExitCode, PreflateConfig, PreflateStreamProcessor, RecreateStreamProcessor,
        };

        // A deflate stream with real matches: compress repetitive-ish
        // bytes with miniz (deterministic in-process compressor, test
        // only — production streams come from the wild).
        let plain: Vec<u8> = (0u32..300_000)
            .map(|i| (i % 251) as u8 ^ (i / 997) as u8)
            .collect();
        let compressed = miniz_oxide::deflate::compress_to_vec(&plain, 6);

        // Split with a small window to force multiple frames.
        let config = PreflateConfig {
            verify_compression: false,
            ..PreflateConfig::default()
        };
        let mut split = PreflateStreamProcessor::new(&config);
        let mut blob = Vec::new(); // framed corrections
        let mut plaintext = Vec::new();
        let (mut start, mut end) = (0usize, (64 * 1024).min(compressed.len()));
        while !split.is_done() {
            match split.decompress(&compressed[start..end]) {
                Ok(r) => {
                    let pt = split.plain_text().text().to_vec();
                    split.shrink_to_dictionary();
                    blob.extend_from_slice(&encode_frame_header(FrameHeader {
                        plaintext_len: u32::try_from(pt.len()).expect("test sizes"),
                        corrections_len: u32::try_from(r.corrections.len()).expect("test sizes"),
                    }));
                    blob.extend_from_slice(&r.corrections);
                    plaintext.extend_from_slice(&pt);
                    start += r.compressed_size;
                    end = (start + 64 * 1024).min(compressed.len());
                }
                Err(e) if e.exit_code() == ExitCode::ShortRead && end < compressed.len() => {
                    end = (end + 64 * 1024).min(compressed.len());
                }
                Err(e) => panic!("split failed: {e:?}"),
            }
        }
        assert_eq!(plaintext, plain, "split plaintext is the member plaintext");

        // Recreate the way the guest does: walk frames, feed recompress.
        let mut recreate = RecreateStreamProcessor::new();
        let mut rebuilt = Vec::new();
        let mut cursor = 0usize;
        let mut pt_cursor = 0usize;
        while cursor < blob.len() {
            let header = parse_frame_header(&blob[cursor..cursor + 8])
                .expect("well-formed")
                .expect("non-empty");
            cursor += 8;
            let corrections = &blob[cursor..cursor + header.corrections_len as usize];
            cursor += header.corrections_len as usize;
            let pt = &plaintext[pt_cursor..pt_cursor + header.plaintext_len as usize];
            pt_cursor += header.plaintext_len as usize;
            let (bytes, _) = recreate
                .recompress(&mut Cursor::new(pt), corrections)
                .expect("recreate");
            rebuilt.extend_from_slice(&bytes);
        }
        assert_eq!(pt_cursor, plaintext.len(), "frames partition the plaintext");
        assert_eq!(rebuilt, compressed, "bit-exact recreation");
    }
}
