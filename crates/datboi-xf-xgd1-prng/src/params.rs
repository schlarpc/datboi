//! The `fill` op's params schema (owned by this component's hash, D89):
//! a strict canonical-CBOR map `{1: seed, 2: sectors}` — the 32-bit
//! seed and the number of 2048-byte filler sectors the stream claims.
//!
//! Hand-rolled on purpose: the schema is two unsigned integers, frozen
//! with the component, and both sides (the native analyzer minting the
//! recipe and the guest executing it) must agree byte-for-byte on the
//! ONE encoding — a non-canonical params bstr is a deterministic
//! refusal, never something to normalize (docs/recipes.md).

/// Decoded `fill` params.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// The 32-bit mastering seed.
    pub seed: u32,
    /// Filler sectors in the stream (2048 bytes each). Bounded by
    /// `u32::MAX` — the stream position is a 32-bit sector index.
    pub sectors: u64,
}

/// Longest possible encoding: map head + two (key, 9-byte uint) pairs.
pub const MAX_ENCODED: usize = 1 + 2 * (1 + 9);

const KEY_SEED: u64 = 1;
const KEY_SECTORS: u64 = 2;

impl Params {
    /// Encode to the canonical bytes a recipe carries. Returns the
    /// buffer and the encoded length (no allocation: the solver crate
    /// stays dependency-free).
    #[must_use]
    pub fn encode(&self) -> ([u8; MAX_ENCODED], usize) {
        let mut out = [0u8; MAX_ENCODED];
        let mut n = 0;
        out[n] = 0xa2; // map, 2 entries
        n += 1;
        n += put_head(&mut out[n..], 0, KEY_SEED);
        n += put_head(&mut out[n..], 0, u64::from(self.seed));
        n += put_head(&mut out[n..], 0, KEY_SECTORS);
        n += put_head(&mut out[n..], 0, self.sectors);
        (out, n)
    }

    /// Decode, refusing anything but the one canonical encoding.
    ///
    /// # Errors
    /// A static reason: wrong shape, non-minimal integer heads, keys
    /// out of order or unknown, a seed above 32 bits, a sector count
    /// above 32 bits, or trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        let mut pos = 0usize;
        if bytes.first() != Some(&0xa2) {
            return Err("params must be a 2-entry map");
        }
        pos += 1;
        let (k, adv) = take_uint(&bytes[pos..])?;
        pos += adv;
        if k != KEY_SEED {
            return Err("params: first key must be 1 (seed)");
        }
        let (seed, adv) = take_uint(&bytes[pos..])?;
        pos += adv;
        let seed = u32::try_from(seed).map_err(|_| "params: seed exceeds 32 bits")?;
        let (k, adv) = take_uint(&bytes[pos..])?;
        pos += adv;
        if k != KEY_SECTORS {
            return Err("params: second key must be 2 (sectors)");
        }
        let (sectors, adv) = take_uint(&bytes[pos..])?;
        pos += adv;
        if sectors > u64::from(u32::MAX) {
            return Err("params: sector count exceeds the 32-bit stream index");
        }
        if pos != bytes.len() {
            return Err("params: trailing bytes");
        }
        Ok(Self { seed, sectors })
    }
}

/// Shortest-form head (RFC 8949 §4.2.1); returns bytes written.
fn put_head(out: &mut [u8], major: u8, n: u64) -> usize {
    let major = major << 5;
    match n {
        0..=23 => {
            out[0] = major | (n as u8);
            1
        }
        24..=0xff => {
            out[0] = major | 24;
            out[1] = n as u8;
            2
        }
        0x100..=0xffff => {
            out[0] = major | 25;
            out[1..3].copy_from_slice(&(n as u16).to_be_bytes());
            3
        }
        0x1_0000..=0xffff_ffff => {
            out[0] = major | 26;
            out[1..5].copy_from_slice(&(n as u32).to_be_bytes());
            5
        }
        _ => {
            out[0] = major | 27;
            out[1..9].copy_from_slice(&n.to_be_bytes());
            9
        }
    }
}

/// One unsigned integer in canonical form; returns (value, bytes consumed).
fn take_uint(bytes: &[u8]) -> Result<(u64, usize), &'static str> {
    let &head = bytes.first().ok_or("params: truncated")?;
    if head >> 5 != 0 {
        return Err("params: expected an unsigned integer");
    }
    let info = head & 0x1f;
    let (value, adv) = match info {
        0..=23 => (u64::from(info), 1),
        24 => (u64::from(*bytes.get(1).ok_or("params: truncated")?), 2),
        25 => {
            let b = bytes.get(1..3).ok_or("params: truncated")?;
            (u64::from(u16::from_be_bytes([b[0], b[1]])), 3)
        }
        26 => {
            let b = bytes.get(1..5).ok_or("params: truncated")?;
            (u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])), 5)
        }
        27 => {
            let b = bytes.get(1..9).ok_or("params: truncated")?;
            let mut a = [0u8; 8];
            a.copy_from_slice(b);
            (u64::from_be_bytes(a), 9)
        }
        _ => return Err("params: malformed integer head"),
    };
    // Canonical rule: the shortest head that fits.
    let minimal = match value {
        0..=23 => 1,
        24..=0xff => 2,
        0x100..=0xffff => 3,
        0x1_0000..=0xffff_ffff => 5,
        _ => 9,
    };
    if adv != minimal {
        return Err("params: non-minimal integer encoding");
    }
    Ok((value, adv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_is_canonical() {
        for (seed, sectors) in [
            (0u32, 0u64),
            (23, 24),
            (0x4E99_8EB0, 1_595_642),
            (u32::MAX, u64::from(u32::MAX)),
        ] {
            let p = Params { seed, sectors };
            let (buf, n) = p.encode();
            assert_eq!(Params::decode(&buf[..n]), Ok(p));
        }
        // {1: 0x4E998EB0, 2: 1000}: a2 01 1a 4e 99 8e b0 02 19 03 e8
        let (buf, n) = Params {
            seed: 0x4E99_8EB0,
            sectors: 1000,
        }
        .encode();
        assert_eq!(
            &buf[..n],
            &[
                0xa2, 0x01, 0x1a, 0x4e, 0x99, 0x8e, 0xb0, 0x02, 0x19, 0x03, 0xe8
            ]
        );
    }

    #[test]
    fn refuses_non_canonical_and_malformed() {
        // Non-minimal seed (0 encoded with a 1-byte argument).
        assert!(Params::decode(&[0xa2, 0x01, 0x18, 0x00, 0x02, 0x01]).is_err());
        // Keys out of order.
        assert!(Params::decode(&[0xa2, 0x02, 0x01, 0x01, 0x01]).is_err());
        // Trailing byte.
        assert!(Params::decode(&[0xa2, 0x01, 0x01, 0x02, 0x01, 0x00]).is_err());
        // Sector count beyond the 32-bit stream index.
        assert!(Params::decode(&[0xa2, 0x01, 0x01, 0x02, 0x1b, 0, 0, 0, 1, 0, 0, 0, 0]).is_err());
        // Wrong container.
        assert!(Params::decode(&[0x82, 0x01, 0x01]).is_err());
        assert!(Params::decode(&[]).is_err());
    }
}
