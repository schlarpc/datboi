//! Reference transform: the swap family (bitswap/byteswap/wordswap), the
//! wasm twin of the `swap@1` builtin (docs/70-recipes.md).
//!
//! Purpose: prove the ABI and the determinism gate (M1 prototype 3), not to
//! be useful — the native builtin serves production traffic (D6). The pure
//! functions below are spec-pinned and target-independent (tested on the
//! host); the `component` module wires them to the frozen
//! `datboi:transform@1` world and only compiles for `wasm32`.

/// Reverse the bits of every byte (skipper `bitswap`).
pub fn bitswap(buf: &mut [u8]) {
    for b in buf {
        *b = b.reverse_bits();
    }
}

/// Swap bytes within 16-bit words (skipper `byteswap`, N64 v64↔z64).
/// Trailing odd byte is left untouched, matching detector semantics.
pub fn byteswap16(buf: &mut [u8]) {
    for pair in buf.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
}

/// Reverse each 32-bit dword (skipper `wordswap`: 01|02|03|04 → 04|03|02|01).
pub fn wordswap32(buf: &mut [u8]) {
    for quad in buf.chunks_exact_mut(4) {
        quad.reverse();
    }
}

/// Swap 16-bit words within 32-bit dwords without swapping their bytes
/// (skipper `wordbyteswap`: 01|02|03|04 → 03|04|01|02).
pub fn wordbyteswap32(buf: &mut [u8]) {
    for quad in buf.chunks_exact_mut(4) {
        quad.swap(0, 2);
        quad.swap(1, 3);
    }
}

/// Apply the swap named by `op` to a fresh copy of `input`. Shared by the
/// wasm component and the host-side tests so both agree by construction.
/// Returns `None` for an unknown op (the world's `run` maps that to Err).
#[must_use]
pub fn apply(op: &str, input: &[u8]) -> Option<Vec<u8>> {
    let mut buf = input.to_vec();
    match op {
        "bitswap" => bitswap(&mut buf),
        "byteswap" => byteswap16(&mut buf),
        "wordswap" => wordswap32(&mut buf),
        "wordbyteswap" => wordbyteswap32(&mut buf),
        _ => return None,
    }
    Some(buf)
}

/// Component glue for the frozen `datboi:transform@1` world. Only built for
/// `wasm32` so the host-side unit tests (run natively by `nix flake check`)
/// don't drag in the component machinery. `unsafe_code` is allowed here
/// because the generated component ABI shims require it; our own logic
/// (`super::apply`) stays safe.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod component {
    wit_bindgen::generate!({
        world: "transform",
        path: "../wit",
    });

    // `generate!` hoists `Descriptor` (it appears in the world's function
    // signatures) to this module's root; `SeekClass` only appears nested, so
    // it's imported from the generated interface path.
    use datboi::transform::types::SeekClass;

    struct Xf;

    impl Guest for Xf {
        /// Every swap is byte-for-byte positional → affine, no random access.
        fn describe(_op: String) -> Descriptor {
            Descriptor {
                seek: SeekClass::Affine,
                random_access_inputs: Vec::new(),
            }
        }

        /// One input blob in, one swapped output blob out. `params` is unused
        /// by the swap ops (the operation is fully determined by `op`).
        fn run(op: String, _params: Vec<u8>, inputs: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, String> {
            let [input] = <[Vec<u8>; 1]>::try_from(inputs)
                .map_err(|got| format!("swap takes exactly 1 input, got {}", got.len()))?;
            let out = super::apply(&op, &input).ok_or_else(|| format!("unknown swap op {op:?}"))?;
            Ok(vec![out])
        }
    }

    export!(Xf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_are_involutions() {
        let original: Vec<u8> = (0..=255).collect();
        for f in [bitswap, byteswap16, wordswap32, wordbyteswap32] {
            let mut buf = original.clone();
            f(&mut buf);
            assert_ne!(buf, original);
            f(&mut buf);
            assert_eq!(buf, original);
        }
    }

    #[test]
    fn byteswap_matches_n64_semantics() {
        let mut buf = vec![0x80, 0x37, 0x12, 0x40, 0xff];
        byteswap16(&mut buf);
        assert_eq!(buf, [0x37, 0x80, 0x40, 0x12, 0xff]);
    }

    /// Semantics pinned to the clrmamepro spec's own example bytes — these
    /// names feed the swap@1 builtin and must never drift (D5).
    #[test]
    fn word_ops_match_clrmamepro_spec() {
        let mut ws = vec![0x01, 0x02, 0x03, 0x04];
        wordswap32(&mut ws);
        assert_eq!(ws, [0x04, 0x03, 0x02, 0x01]);

        let mut wbs = vec![0x01, 0x02, 0x03, 0x04];
        wordbyteswap32(&mut wbs);
        assert_eq!(wbs, [0x03, 0x04, 0x01, 0x02]);
    }

    #[test]
    fn apply_dispatches_and_rejects_unknown() {
        assert_eq!(apply("byteswap", &[0x01, 0x02]), Some(vec![0x02, 0x01]));
        assert_eq!(apply("nonsense", &[0x01]), None);
    }
}
