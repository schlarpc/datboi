//! Wii partition body encryption (D116).
//!
//! # The format
//!
//! A Wii disc partition's body is a run of 32 KiB sectors. Each sector
//! is a 1 KiB hash block followed by 31 KiB of data, both AES-128-CBC
//! encrypted under the partition's title key: the hash block with a zero
//! IV, the data with the LAST 16 BYTES OF THE ENCRYPTED H2 AREA
//! (`0x3D0..0x3E0` of the encrypted block) as its IV. The hash block is
//! a three-level SHA-1 tree over the plaintext data:
//!
//! * **H0** — 31 hashes, one per 1 KiB of the sector's data, at `0`;
//! * **H1** — 8 hashes, one per sector of the 8-sector SUBGROUP the
//!   sector belongs to (each = SHA-1 of that sector's 31 H0s), at
//!   `0x280`;
//! * **H2** — 8 hashes, one per subgroup of the 64-sector GROUP (each =
//!   SHA-1 of that subgroup's 8 H1s), at `0x340`;
//!
//! with zero padding between and after (`0x26C..0x280`, `0x320..0x340`,
//! `0x3E0..0x400`). The H3 table in the partition header carries one
//! SHA-1 per group (of its 8 H2s); it is a separate plaintext structure
//! this crate neither reads nor writes.
//!
//! So encryption is a pure function of the plaintext data and the key,
//! and its natural unit is the 2 MiB group: a sector's ciphertext needs
//! the H2s, which need every sector in its group. That is the seek
//! quantum `encrypt` serves ranges at; `decrypt` is sector-granular
//! (each sector carries its own IV in its own bytes).
//!
//! The title key is wrapped: the ticket stores it AES-128-CBC encrypted
//! under a console COMMON key with the title ID (zero-padded) as IV.
//! The common key is a recipe INPUT (D12: keys are ordinary blobs,
//! never distributed with the software); the wrapped key and title ID
//! ride in the params, where they were already public on the disc.
//!
//! # What this crate is
//!
//! The component's `encrypt` / `decrypt` ops (see `component`, wasm32
//! only) and the native twin the analyzer verifies with: every
//! partition it claims was decrypted here, its hash tree recomputed and
//! compared to the disc's own hash blocks byte for byte, before any
//! recipe is minted (D111's verify-at-discovery rule). References:
//! WiiBrew's "Wii disc" page and the `nod` crate's reader (MIT OR
//! Apache-2.0).

#![deny(unsafe_code)]
#![deny(missing_docs)]

use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use sha1::{Digest, Sha1 as Sha1Hasher};

/// Bytes per encrypted sector.
pub const SECTOR_LEN: usize = 0x8000;
/// Bytes of hash block at the head of every sector.
pub const HASHES_LEN: usize = 0x400;
/// Plaintext data bytes per sector.
pub const DATA_LEN: usize = SECTOR_LEN - HASHES_LEN;
/// Sectors per hash group — the encrypt seek quantum.
pub const GROUP_SECTORS: usize = 64;
/// Sectors per H1 subgroup.
pub const SUBGROUP_SECTORS: usize = 8;
/// H0 hashes per sector (one per 1 KiB of data).
pub const H0_PER_SECTOR: usize = DATA_LEN / HASHES_LEN;
/// Plaintext bytes per group.
pub const GROUP_DATA_LEN: usize = DATA_LEN * GROUP_SECTORS;
/// Ciphertext bytes per group.
pub const GROUP_LEN: usize = SECTOR_LEN * GROUP_SECTORS;
/// Offset of the H1 hashes inside a hash block.
pub const H1_OFFSET: usize = 0x280;
/// Offset of the H2 hashes inside a hash block.
pub const H2_OFFSET: usize = 0x340;
/// The data IV is this window of the ENCRYPTED hash block.
pub const DATA_IV_OFFSET: usize = 0x3D0;

/// An AES-128 key.
pub type Key = [u8; 16];
/// A SHA-1 digest.
pub type Sha1 = [u8; 20];

const SHA1_LEN: usize = 20;

/// AES-128-CBC over whole blocks (no padding), in place.
///
/// # Panics
/// `data.len()` must be a multiple of 16.
pub fn cbc_encrypt(key: &Key, iv: &Key, data: &mut [u8]) {
    assert_eq!(data.len() % 16, 0, "cbc over whole blocks only");
    let cipher = Aes128::new(key.into());
    let mut prev = *iv;
    for block in data.chunks_exact_mut(16) {
        for (b, p) in block.iter_mut().zip(&prev) {
            *b ^= p;
        }
        cipher.encrypt_block(block.into());
        prev.copy_from_slice(block);
    }
}

/// AES-128-CBC over whole blocks (no padding), in place.
///
/// # Panics
/// `data.len()` must be a multiple of 16.
pub fn cbc_decrypt(key: &Key, iv: &Key, data: &mut [u8]) {
    assert_eq!(data.len() % 16, 0, "cbc over whole blocks only");
    let cipher = Aes128::new(key.into());
    let mut prev = *iv;
    let mut ct = [0u8; 16];
    for block in data.chunks_exact_mut(16) {
        ct.copy_from_slice(block);
        cipher.decrypt_block(block.into());
        for (b, p) in block.iter_mut().zip(&prev) {
            *b ^= p;
        }
        prev = ct;
    }
}

/// Unwrap a ticket's title key: AES-128-CBC decrypt under the common
/// key with the title ID (zero-padded to 16 bytes) as IV.
#[must_use]
pub fn title_key(common: &Key, wrapped: &Key, title_id: &[u8; 8]) -> Key {
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(title_id);
    let mut key = *wrapped;
    cbc_decrypt(common, &iv, &mut key);
    key
}

fn sha1(bytes: &[u8]) -> Sha1 {
    let mut h = Sha1Hasher::new();
    h.update(bytes);
    h.finalize().into()
}

/// The hash tree of one group, computed from plaintext data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupHashes {
    /// `GROUP_SECTORS * H0_PER_SECTOR` entries, sector-major.
    pub h0: Vec<Sha1>,
    /// One per sector of the group.
    pub h1: [Sha1; GROUP_SECTORS],
    /// One per subgroup.
    pub h2: [Sha1; GROUP_SECTORS / SUBGROUP_SECTORS],
    /// SHA-1 of the H2s — the group's entry in the partition's H3 table.
    pub h3: Sha1,
}

impl GroupHashes {
    /// Hash a group's plaintext: `data` is up to [`GROUP_DATA_LEN`] bytes
    /// in whole [`DATA_LEN`] sectors. A partition whose sector count is
    /// not a multiple of 64 ends in a partial group; the sectors it
    /// does not have hash as zero data (the mastering tool's convention,
    /// which the disc's own hash blocks confirm or deny by equality).
    ///
    /// # Panics
    /// `data.len()` must be a multiple of [`DATA_LEN`] and at most a group.
    #[must_use]
    pub fn compute(data: &[u8]) -> Self {
        assert_eq!(data.len() % DATA_LEN, 0, "whole sectors");
        assert!(data.len() <= GROUP_DATA_LEN, "at most one group");
        let present = data.len() / DATA_LEN;
        let zero_h0 = sha1(&[0u8; HASHES_LEN]);
        let mut h0 = vec![[0u8; SHA1_LEN]; GROUP_SECTORS * H0_PER_SECTOR];
        let mut h1 = [[0u8; SHA1_LEN]; GROUP_SECTORS];
        for sector in 0..GROUP_SECTORS {
            let h0s = &mut h0[sector * H0_PER_SECTOR..(sector + 1) * H0_PER_SECTOR];
            if sector < present {
                let d = &data[sector * DATA_LEN..(sector + 1) * DATA_LEN];
                for (i, out) in h0s.iter_mut().enumerate() {
                    *out = sha1(&d[i * HASHES_LEN..(i + 1) * HASHES_LEN]);
                }
            } else {
                h0s.fill(zero_h0);
            }
            h1[sector] = sha1(h0s.as_flattened());
        }
        let mut h2 = [[0u8; SHA1_LEN]; GROUP_SECTORS / SUBGROUP_SECTORS];
        for (sub, out) in h2.iter_mut().enumerate() {
            *out = sha1(h1[sub * SUBGROUP_SECTORS..(sub + 1) * SUBGROUP_SECTORS].as_flattened());
        }
        let h3 = sha1(h2.as_flattened());
        Self { h0, h1, h2, h3 }
    }

    /// The plaintext hash block of `sector` (index within the group),
    /// zero padding included.
    pub fn block(&self, sector: usize, out: &mut [u8; HASHES_LEN]) {
        out.fill(0);
        let h0 = &self.h0[sector * H0_PER_SECTOR..(sector + 1) * H0_PER_SECTOR];
        out[..H0_PER_SECTOR * SHA1_LEN].copy_from_slice(h0.as_flattened());
        let sub = sector / SUBGROUP_SECTORS;
        let h1 = &self.h1[sub * SUBGROUP_SECTORS..(sub + 1) * SUBGROUP_SECTORS];
        out[H1_OFFSET..H1_OFFSET + SUBGROUP_SECTORS * SHA1_LEN].copy_from_slice(h1.as_flattened());
        out[H2_OFFSET..H2_OFFSET + self.h2.len() * SHA1_LEN]
            .copy_from_slice(self.h2.as_flattened());
    }
}

/// Encrypt one sector: its hash block (from the group's tree) under a
/// zero IV, then its data under the IV the encrypted block yields.
pub fn encrypt_sector(
    key: &Key,
    hashes: &GroupHashes,
    sector: usize,
    data: &[u8],
    out: &mut [u8; SECTOR_LEN],
) {
    assert_eq!(data.len(), DATA_LEN, "one sector of data");
    let (block, body) = out.split_at_mut(HASHES_LEN);
    let block: &mut [u8; HASHES_LEN] = block.try_into().expect("split at HASHES_LEN");
    hashes.block(sector, block);
    cbc_encrypt(key, &[0u8; 16], block);
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&block[DATA_IV_OFFSET..DATA_IV_OFFSET + 16]);
    body.copy_from_slice(data);
    cbc_encrypt(key, &iv, body);
}

/// Decrypt one sector in full — hash block and data — into `out`.
pub fn decrypt_sector(key: &Key, enc: &[u8; SECTOR_LEN], out: &mut [u8; SECTOR_LEN]) {
    out.copy_from_slice(enc);
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&enc[DATA_IV_OFFSET..DATA_IV_OFFSET + 16]);
    let (block, body) = out.split_at_mut(HASHES_LEN);
    cbc_decrypt(key, &[0u8; 16], block);
    cbc_decrypt(key, &iv, body);
}

/// Decrypt only a sector's data (the hash block is left undecrypted).
pub fn decrypt_data(key: &Key, enc: &[u8; SECTOR_LEN], out: &mut [u8]) {
    assert_eq!(out.len(), DATA_LEN, "one sector of data");
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&enc[DATA_IV_OFFSET..DATA_IV_OFFSET + 16]);
    out.copy_from_slice(&enc[HASHES_LEN..]);
    cbc_decrypt(key, &iv, out);
}

/// Does a decrypted sector's hash block equal, byte for byte, what the
/// group's plaintext yields? The whole 1 KiB is compared — padding
/// included — because that is what re-encryption reproduces.
#[must_use]
pub fn hash_block_matches(plain: &[u8; SECTOR_LEN], hashes: &GroupHashes, sector: usize) -> bool {
    let mut expect = [0u8; HASHES_LEN];
    hashes.block(sector, &mut expect);
    plain[..HASHES_LEN] == expect
}

pub mod params;

#[cfg(target_arch = "wasm32")]
mod component;

#[cfg(test)]
mod tests;
