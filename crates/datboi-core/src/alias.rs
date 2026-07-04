//! Single-pass alias hashing (D2/D22): ingest computes the full dat-hash
//! tuple (crc32/md5/sha1/sha256) alongside the native blake3 in one
//! streaming pass. Aliases are lookup hints; blake3 is truth.

use sha1::Digest as _;

use crate::hash::Blake3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AliasTuple {
    pub size: u64,
    pub crc32: [u8; 4],
    pub md5: [u8; 16],
    pub sha1: [u8; 20],
    pub sha256: [u8; 32],
    pub blake3: Blake3,
}

pub struct AliasHasher {
    crc32: crc32fast::Hasher,
    md5: md5::Md5,
    sha1: sha1::Sha1,
    sha256: sha2::Sha256,
    blake3: blake3::Hasher,
    size: u64,
}

impl AliasHasher {
    #[must_use]
    pub fn new() -> Self {
        Self {
            crc32: crc32fast::Hasher::new(),
            md5: md5::Md5::new(),
            sha1: sha1::Sha1::new(),
            sha256: sha2::Sha256::new(),
            blake3: blake3::Hasher::new(),
            size: 0,
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.crc32.update(bytes);
        self.md5.update(bytes);
        self.sha1.update(bytes);
        self.sha256.update(bytes);
        self.blake3.update(bytes);
        self.size += bytes.len() as u64;
    }

    #[must_use]
    pub fn finalize(self) -> AliasTuple {
        AliasTuple {
            size: self.size,
            crc32: self.crc32.finalize().to_be_bytes(),
            md5: self.md5.finalize().into(),
            sha1: self.sha1.finalize().into(),
            sha256: self.sha256.finalize().into(),
            blake3: Blake3(*self.blake3.finalize().as_bytes()),
        }
    }
}

impl Default for AliasHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        assert!(s.len().is_multiple_of(2));
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn tuple_of(chunks: &[&[u8]]) -> AliasTuple {
        let mut hasher = AliasHasher::new();
        for chunk in chunks {
            hasher.update(chunk);
        }
        hasher.finalize()
    }

    #[test]
    fn known_answers_empty_input() {
        let t = tuple_of(&[]);
        assert_eq!(t.size, 0);
        assert_eq!(t.crc32, [0, 0, 0, 0]);
        assert_eq!(t.md5.as_slice(), hex("d41d8cd98f00b204e9800998ecf8427e"));
        assert_eq!(
            t.sha1.as_slice(),
            hex("da39a3ee5e6b4b0d3255bfef95601890afd80709")
        );
        assert_eq!(
            t.sha256.as_slice(),
            hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(
            t.blake3.to_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn known_answers_abc() {
        // Chunked update must equal one-shot (streaming correctness).
        for t in [tuple_of(&[b"abc"]), tuple_of(&[b"a", b"", b"bc"])] {
            assert_eq!(t.size, 3);
            assert_eq!(t.crc32, hex("352441c2").as_slice());
            assert_eq!(t.md5.as_slice(), hex("900150983cd24fb0d6963f7d28e17f72"));
            assert_eq!(
                t.sha1.as_slice(),
                hex("a9993e364706816aba3e25717850c26c9cd0d89d")
            );
            assert_eq!(
                t.sha256.as_slice(),
                hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
            );
            assert_eq!(
                t.blake3.to_hex(),
                "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
            );
        }
    }
}
