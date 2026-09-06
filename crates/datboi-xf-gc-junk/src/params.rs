//! The `fill` op's params schema (owned by this component's hash, D89):
//! a strict canonical-CBOR map `{1: id, 2: disc, 3: len}` — the first
//! four bytes of the game ID packed big-endian into a u32, the disc
//! number, and the stream length in bytes (the disc's address space).
//!
//! Hand-rolled on purpose, like xf-xgd1-prng's: the schema is three
//! unsigned integers frozen with the component, and both sides must
//! agree byte-for-byte on the ONE encoding (docs/recipes.md).

/// Decoded `fill` params.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// The game ID's first four bytes (`GALE` → `0x47414C45`).
    pub id: [u8; 4],
    /// Disc number (0 for the first disc).
    pub disc: u8,
    /// Stream length in bytes. Bounded by `u32::MAX` sectors of 32 KiB.
    pub len: u64,
}

/// Longest possible encoding: map head + three (key, 9-byte uint) pairs.
pub const MAX_ENCODED: usize = 1 + 3 * (1 + 9);
/// The longest stream a 32-bit sector index addresses.
pub const MAX_LEN: u64 = (u32::MAX as u64 + 1) * crate::SECTOR_LEN as u64;

const KEY_ID: u64 = 1;
const KEY_DISC: u64 = 2;
const KEY_LEN: u64 = 3;

impl Params {
    /// Encode to the canonical bytes a recipe carries. Returns the
    /// buffer and the encoded length (no allocation).
    #[must_use]
    pub fn encode(&self) -> ([u8; MAX_ENCODED], usize) {
        let mut out = [0u8; MAX_ENCODED];
        let mut n = 0;
        out[n] = 0xa3; // map, 3 entries
        n += 1;
        n += put_head(&mut out[n..], KEY_ID);
        n += put_head(&mut out[n..], u64::from(u32::from_be_bytes(self.id)));
        n += put_head(&mut out[n..], KEY_DISC);
        n += put_head(&mut out[n..], u64::from(self.disc));
        n += put_head(&mut out[n..], KEY_LEN);
        n += put_head(&mut out[n..], self.len);
        (out, n)
    }

    /// Decode, refusing anything but the one canonical encoding.
    ///
    /// # Errors
    /// A static reason: wrong shape, non-minimal integer heads, keys
    /// out of order or unknown, an id above 32 bits, a disc number
    /// above 8 bits, a length past the 32-bit sector index, or trailing
    /// bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        let mut pos = 0usize;
        if bytes.first() != Some(&0xa3) {
            return Err("params must be a 3-entry map");
        }
        pos += 1;
        let mut key = |want: u64, what: &'static str| -> Result<u64, &'static str> {
            let (k, adv) = take_uint(&bytes[pos..])?;
            pos += adv;
            if k != want {
                return Err(what);
            }
            let (v, adv) = take_uint(&bytes[pos..])?;
            pos += adv;
            Ok(v)
        };
        let id = key(KEY_ID, "params: first key must be 1 (id)")?;
        let id = u32::try_from(id).map_err(|_| "params: id exceeds 32 bits")?;
        let disc = key(KEY_DISC, "params: second key must be 2 (disc)")?;
        let disc = u8::try_from(disc).map_err(|_| "params: disc exceeds 8 bits")?;
        let len = key(KEY_LEN, "params: third key must be 3 (len)")?;
        if len > MAX_LEN {
            return Err("params: length exceeds the 32-bit sector index");
        }
        if pos != bytes.len() {
            return Err("params: trailing bytes");
        }
        Ok(Self {
            id: id.to_be_bytes(),
            disc,
            len,
        })
    }
}

/// Shortest-form unsigned head (RFC 8949 §4.2.1); returns bytes written.
fn put_head(out: &mut [u8], n: u64) -> usize {
    match n {
        0..=23 => {
            out[0] = n as u8;
            1
        }
        24..=0xff => {
            out[0] = 24;
            out[1] = n as u8;
            2
        }
        0x100..=0xffff => {
            out[0] = 25;
            out[1..3].copy_from_slice(&(n as u16).to_be_bytes());
            3
        }
        0x1_0000..=0xffff_ffff => {
            out[0] = 26;
            out[1..5].copy_from_slice(&(n as u32).to_be_bytes());
            5
        }
        _ => {
            out[0] = 27;
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
        for (id, disc, len) in [
            (*b"\0\0\0\0", 0u8, 0u64),
            (*b"GALE", 0, 1_459_978_240),
            (*b"GM8E", 1, 24),
            (*b"\xff\xff\xff\xff", u8::MAX, MAX_LEN),
        ] {
            let p = Params { id, disc, len };
            let (buf, n) = p.encode();
            assert_eq!(Params::decode(&buf[..n]), Ok(p));
        }
        // {1: 0x47414C45, 2: 0, 3: 1000}
        let (buf, n) = Params {
            id: *b"GALE",
            disc: 0,
            len: 1000,
        }
        .encode();
        assert_eq!(
            &buf[..n],
            &[
                0xa3, 0x01, 0x1a, 0x47, 0x41, 0x4c, 0x45, 0x02, 0x00, 0x03, 0x19, 0x03, 0xe8
            ]
        );
    }

    #[test]
    fn refuses_non_canonical_and_malformed() {
        // Non-minimal id head.
        assert!(Params::decode(&[0xa3, 0x01, 0x18, 0x00, 0x02, 0x00, 0x03, 0x01]).is_err());
        // Keys out of order.
        assert!(Params::decode(&[0xa3, 0x02, 0x00, 0x01, 0x01, 0x03, 0x01]).is_err());
        // Disc number above 8 bits.
        assert!(Params::decode(&[0xa3, 0x01, 0x01, 0x02, 0x19, 0x01, 0x00, 0x03, 0x01]).is_err());
        // Trailing byte.
        assert!(Params::decode(&[0xa3, 0x01, 0x01, 0x02, 0x00, 0x03, 0x01, 0x00]).is_err());
        // Wrong container.
        assert!(Params::decode(&[0xa2, 0x01, 0x01, 0x02, 0x00]).is_err());
        assert!(Params::decode(&[]).is_err());
    }
}
