//! Reference transform: the swap family (bitswap/byteswap/wordswap), the
//! wasm twin of the `swap@1` builtin (docs/70-recipes.md).
//!
//! Purpose: prove the ABI and the determinism gate (M1 prototype 3), not to
//! be useful — the native builtin serves production traffic (D6). Component
//! bindings (wit-bindgen against ../wit/transform.wit) land with the
//! prototype; the pure logic below is target-independent and tested on the
//! host.

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

/// Swap 16-bit words within 32-bit dwords (skipper `wordswap`).
pub fn wordswap32(buf: &mut [u8]) {
    for quad in buf.chunks_exact_mut(4) {
        quad.swap(0, 2);
        quad.swap(1, 3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_are_involutions() {
        let original: Vec<u8> = (0..=255).collect();
        for f in [bitswap, byteswap16, wordswap32] {
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
}
