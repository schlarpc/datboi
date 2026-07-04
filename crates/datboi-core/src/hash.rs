//! The native content address (D2): blake3, fixed, no multihash.

use std::fmt;
use std::str::FromStr;

/// A blake3 hash of an object's exact bytes. The one true identity (D2);
/// dat hashes (crc32/md5/sha1/sha256) are aliases in the metadata DB, never keys.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Blake3(pub [u8; 32]);

impl Blake3 {
    #[must_use]
    pub fn compute(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            use fmt::Write;
            write!(s, "{b:02x}").expect("writing to String cannot fail");
        }
        s
    }
}

impl fmt::Display for Blake3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Blake3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Blake3({})", self.to_hex())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseHashError {
    #[error("expected 64 hex chars, got {0}")]
    Length(usize),
    #[error("invalid hex at byte {0}")]
    Hex(usize),
}

impl FromStr for Blake3 {
    type Err = ParseHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 64 {
            return Err(ParseHashError::Length(s.len()));
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
            let hi = hex_val(chunk[0]).ok_or(ParseHashError::Hex(i * 2))?;
            let lo = hex_val(chunk[1]).ok_or(ParseHashError::Hex(i * 2 + 1))?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let h = Blake3::compute(b"datboi");
        let parsed: Blake3 = h.to_hex().parse().expect("round trip");
        assert_eq!(h, parsed);
    }

    #[test]
    fn rejects_uppercase_and_bad_lengths() {
        assert_eq!(
            "AB".repeat(32).parse::<Blake3>(),
            Err(ParseHashError::Hex(0))
        );
        assert_eq!("ab".parse::<Blake3>(), Err(ParseHashError::Length(2)));
    }
}
