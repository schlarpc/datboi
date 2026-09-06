//! The `encrypt` / `decrypt` params schema (owned by this component's
//! hash, D89): a strict canonical-CBOR map `{1: wrapped title key
//! (16-byte bstr), 2: title id (8-byte bstr), 3: sectors}` — the
//! ticket's encrypted title key and the title ID that is its unwrap IV
//! (both public on the disc), and the partition body's length in
//! 32 KiB sectors. Both ops take the same params: one describes the
//! partition, the direction is the op name.
//!
//! Hand-rolled on purpose, like xf-gc-junk's: the schema is two byte
//! strings and an integer frozen with the component, and both sides
//! must agree byte-for-byte on the ONE encoding (docs/recipes.md).

/// Decoded params.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// The ticket's title key, still wrapped under the common key.
    pub wrapped_title_key: [u8; 16],
    /// The title ID — the unwrap IV's first eight bytes.
    pub title_id: [u8; 8],
    /// Partition body length in 32 KiB sectors (≥ 1).
    pub sectors: u64,
}

/// Longest possible encoding: map head + key/bstr16 + key/bstr8 + key/9-byte uint.
pub const MAX_ENCODED: usize = 1 + (1 + 17) + (1 + 9) + (1 + 9);
/// The longest partition a 32-bit sector index addresses.
pub const MAX_SECTORS: u64 = u32::MAX as u64;

const KEY_TITLE_KEY: u64 = 1;
const KEY_TITLE_ID: u64 = 2;
const KEY_SECTORS: u64 = 3;
const MAJOR_BSTR: u8 = 0x40;

impl Params {
    /// Ciphertext length the params describe.
    #[must_use]
    pub fn encrypted_len(&self) -> u64 {
        self.sectors * crate::SECTOR_LEN as u64
    }

    /// Plaintext length the params describe.
    #[must_use]
    pub fn plain_len(&self) -> u64 {
        self.sectors * crate::DATA_LEN as u64
    }

    /// Encode to the canonical bytes a recipe carries. Returns the
    /// buffer and the encoded length (no allocation).
    #[must_use]
    pub fn encode(&self) -> ([u8; MAX_ENCODED], usize) {
        let mut out = [0u8; MAX_ENCODED];
        let mut n = 0;
        out[n] = 0xa3; // map, 3 entries
        n += 1;
        n += put_uint(&mut out[n..], KEY_TITLE_KEY);
        out[n] = MAJOR_BSTR | 16;
        n += 1;
        out[n..n + 16].copy_from_slice(&self.wrapped_title_key);
        n += 16;
        n += put_uint(&mut out[n..], KEY_TITLE_ID);
        out[n] = MAJOR_BSTR | 8;
        n += 1;
        out[n..n + 8].copy_from_slice(&self.title_id);
        n += 8;
        n += put_uint(&mut out[n..], KEY_SECTORS);
        n += put_uint(&mut out[n..], self.sectors);
        (out, n)
    }

    /// Decode, refusing anything but the one canonical encoding.
    ///
    /// # Errors
    /// A static reason: wrong shape, non-minimal integer heads, keys
    /// out of order or unknown, byte strings of the wrong length, a
    /// sector count of zero or past the 32-bit index, or trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        let mut pos = 0usize;
        if bytes.first() != Some(&0xa3) {
            return Err("params must be a 3-entry map");
        }
        pos += 1;
        let (k, adv) = take_uint(&bytes[pos..])?;
        pos += adv;
        if k != KEY_TITLE_KEY {
            return Err("params: first key must be 1 (title key)");
        }
        let (tk, adv) = take_bstr(&bytes[pos..], 16, "params: title key must be 16 bytes")?;
        pos += adv;
        let (k, adv) = take_uint(&bytes[pos..])?;
        pos += adv;
        if k != KEY_TITLE_ID {
            return Err("params: second key must be 2 (title id)");
        }
        let (tid, adv) = take_bstr(&bytes[pos..], 8, "params: title id must be 8 bytes")?;
        pos += adv;
        let (k, adv) = take_uint(&bytes[pos..])?;
        pos += adv;
        if k != KEY_SECTORS {
            return Err("params: third key must be 3 (sectors)");
        }
        let (sectors, adv) = take_uint(&bytes[pos..])?;
        pos += adv;
        if sectors == 0 {
            return Err("params: a partition has at least one sector");
        }
        if sectors > MAX_SECTORS {
            return Err("params: sector count exceeds the 32-bit index");
        }
        if pos != bytes.len() {
            return Err("params: trailing bytes");
        }
        let mut wrapped_title_key = [0u8; 16];
        wrapped_title_key.copy_from_slice(tk);
        let mut title_id = [0u8; 8];
        title_id.copy_from_slice(tid);
        Ok(Self {
            wrapped_title_key,
            title_id,
            sectors,
        })
    }
}

/// Shortest-form unsigned head (RFC 8949 §4.2.1); returns bytes written.
fn put_uint(out: &mut [u8], n: u64) -> usize {
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

/// A byte string of exactly `want` bytes (short-form head, the only
/// canonical encoding for lengths under 24).
fn take_bstr<'a>(
    bytes: &'a [u8],
    want: usize,
    wrong: &'static str,
) -> Result<(&'a [u8], usize), &'static str> {
    let &head = bytes.first().ok_or("params: truncated")?;
    if head != MAJOR_BSTR | want as u8 {
        return Err(wrong);
    }
    let body = bytes.get(1..1 + want).ok_or("params: truncated")?;
    Ok((body, 1 + want))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(sectors: u64) -> Params {
        Params {
            wrapped_title_key: [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ],
            title_id: *b"\0\x01\0\0RIVE",
            sectors,
        }
    }

    #[test]
    fn round_trips_and_is_canonical() {
        for sectors in [1u64, 23, 24, 0x100, 0x1_0000, 128_000, MAX_SECTORS] {
            let p = sample(sectors);
            let (buf, n) = p.encode();
            assert_eq!(Params::decode(&buf[..n]), Ok(p));
        }
        let (buf, n) = sample(1000).encode();
        let mut want = vec![0xa3, 0x01, 0x50];
        want.extend_from_slice(&sample(0).wrapped_title_key);
        want.extend_from_slice(&[0x02, 0x48]);
        want.extend_from_slice(&sample(0).title_id);
        want.extend_from_slice(&[0x03, 0x19, 0x03, 0xe8]);
        assert_eq!(&buf[..n], &want[..]);
    }

    #[test]
    fn refuses_non_canonical_and_malformed() {
        let (buf, n) = sample(5).encode();
        let good = &buf[..n];
        // Zero sectors.
        let mut z = good.to_vec();
        *z.last_mut().unwrap() = 0;
        assert!(Params::decode(&z).is_err());
        // Trailing byte.
        let mut t = good.to_vec();
        t.push(0);
        assert!(Params::decode(&t).is_err());
        // Title key of the wrong length.
        let mut w = good.to_vec();
        w[2] = MAJOR_BSTR | 15;
        assert!(Params::decode(&w).is_err());
        // Keys out of order / wrong container / empty.
        let mut o = good.to_vec();
        o[1] = 2;
        assert!(Params::decode(&o).is_err());
        assert!(Params::decode(&[0xa2, 0x01, 0x40]).is_err());
        assert!(Params::decode(&[]).is_err());
        // Non-minimal sector head.
        let mut nm = good.to_vec();
        nm.truncate(n - 1);
        nm.extend_from_slice(&[0x18, 0x05]);
        assert!(Params::decode(&nm).is_err());
    }
}
