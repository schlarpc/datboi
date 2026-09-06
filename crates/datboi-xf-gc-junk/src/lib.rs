//! The GameCube / Wii mastering junk generator (D115).
//!
//! # The generator
//!
//! Nintendo's mastering tool fills the unused space of a GameCube disc
//! (and of a Wii partition) with the output of a lagged Fibonacci
//! generator (`k = 521`, `j = 32`) that is RESEEDED at every 32 KiB
//! sector boundary from three things: the first four bytes of the game
//! ID, the disc number, and the sector index. So junk is a pure
//! function of position — byte `x` of the disc is `LFG(seed(id, disc,
//! x / 32 KiB))[x % 32 KiB]` — with no stream state carried across
//! sectors and nothing consumed by the data in between. A range is
//! arithmetic: seed the sector, skip into it, emit.
//!
//! Reconstructed from Dolphin's `LaggedFibonacciGenerator.cpp` and the
//! WIA/RVZ format notes (CC0-1.0), by way of the `nod` crate's port
//! (MIT OR Apache-2.0); the six test vectors in [`tests`] are theirs.
//!
//! # What this crate is
//!
//! The component's `fill` op (see `component`, wasm32 only) and the
//! native twin the analyzer verifies with: every junk byte an analyzer
//! claims was regenerated here and compared to the disc first (D111's
//! rule, verbatim). The stream's identity is over the disc's whole
//! address space — `fill {id, disc, len}` — so every disc pressed
//! under one game ID and disc number shares one junk blob, never
//! stored.

#![deny(unsafe_code)]
#![deny(missing_docs)]

/// Bytes per junk sector: the generator reseeds at this pitch.
pub const SECTOR_LEN: usize = 0x8000;

const LFG_K: usize = 521;
const LFG_J: usize = 32;
const SEED_WORDS: usize = 17;
const LFG_K_BYTES: usize = LFG_K * 4;

/// The lagged Fibonacci generator, positioned inside one sector.
pub struct Lfg {
    buffer: [u32; LFG_K],
    /// Byte position within the current `buffer` (as bytes).
    position: usize,
}

impl Default for Lfg {
    fn default() -> Self {
        Self {
            buffer: [0u32; LFG_K],
            position: 0,
        }
    }
}

impl Lfg {
    /// The 17-word seed for `(id, disc, sector)`.
    fn seed(out: &mut [u32; SEED_WORDS], id: [u8; 4], disc: u8, sector: u32) {
        let seed = u32::from_be_bytes([
            id[2],
            id[1],
            id[3].wrapping_add(id[2]),
            id[0].wrapping_add(id[1]),
        ]) ^ u32::from(disc);
        let mut n = seed.wrapping_mul(0x260B_CD5) ^ sector.wrapping_mul(0x1EF2_9123);
        for v in out.iter_mut() {
            *v = 0;
            for _ in 0..LFG_J {
                n = n.wrapping_mul(0x5D58_8B65).wrapping_add(1);
                *v = (*v >> 1) | (n & 0x8000_0000);
            }
        }
        out[16] ^= (out[0] >> 9) ^ (out[16] << 23);
    }

    fn init(&mut self) {
        for i in SEED_WORDS..LFG_K {
            self.buffer[i] = (self.buffer[i - SEED_WORDS] << 23)
                ^ (self.buffer[i - SEED_WORDS + 1] >> 9)
                ^ self.buffer[i - 1];
        }
        // Fold the output-time "shift by 18 instead of 16" quirk and
        // the byte order into the state once, so emission is a copy.
        for x in self.buffer.iter_mut() {
            *x = ((*x & 0xFF00_FFFF) | ((*x >> 2) & 0x00FF_0000)).to_be();
        }
        for _ in 0..4 {
            self.forward();
        }
    }

    /// Seed for the sector containing disc byte `offset` and skip to it.
    pub fn seek(&mut self, id: [u8; 4], disc: u8, offset: u64) {
        let sector =
            u32::try_from(offset / SECTOR_LEN as u64).expect("disc offsets fit 32-bit sectors");
        let within = usize::try_from(offset % SECTOR_LEN as u64).expect("< sector");
        let mut seed = [0u32; SEED_WORDS];
        Self::seed(&mut seed, id, disc, sector);
        self.buffer[..SEED_WORDS].copy_from_slice(&seed);
        self.position = 0;
        self.init();
        self.skip(within);
    }

    #[inline(never)]
    fn forward(&mut self) {
        for i in 0..LFG_J {
            self.buffer[i] ^= self.buffer[i + LFG_K - LFG_J];
        }
        for i in LFG_J..LFG_K {
            self.buffer[i] ^= self.buffer[i - LFG_J];
        }
    }

    fn skip(&mut self, n: usize) {
        self.position += n;
        while self.position >= LFG_K_BYTES {
            self.forward();
            self.position -= LFG_K_BYTES;
        }
    }

    /// Emit junk into `buf` from the current position — within ONE
    /// sector; the caller reseeds at the boundary ([`fill_at`]).
    pub fn fill(&mut self, mut buf: &mut [u8]) {
        while !buf.is_empty() {
            while self.position >= LFG_K_BYTES {
                self.forward();
                self.position -= LFG_K_BYTES;
            }
            let word = self.position / 4;
            let byte = self.position % 4;
            // One u32 at a time keeps this unsafe-free; the state is
            // already byte-ordered (see `init`).
            let bytes = self.buffer[word].to_ne_bytes();
            let n = (4 - byte).min(buf.len());
            buf[..n].copy_from_slice(&bytes[byte..byte + n]);
            buf = &mut buf[n..];
            self.position += n;
        }
    }
}

/// Fill `buf` with the junk of disc `(id, disc)` starting at byte
/// `offset`, reseeding at every sector boundary crossed.
pub fn fill_at(lfg: &mut Lfg, id: [u8; 4], disc: u8, mut offset: u64, mut buf: &mut [u8]) {
    while !buf.is_empty() {
        lfg.seek(id, disc, offset);
        let room = SECTOR_LEN - usize::try_from(offset % SECTOR_LEN as u64).expect("< sector");
        let n = room.min(buf.len());
        lfg.fill(&mut buf[..n]);
        buf = &mut buf[n..];
        offset += n as u64;
    }
}

/// How many leading bytes of `buf` (disc bytes at `offset`) equal the
/// junk stream — reseeding across sector boundaries, stopping at the
/// first mismatch.
pub fn matching_prefix(
    lfg: &mut Lfg,
    id: [u8; 4],
    disc: u8,
    mut offset: u64,
    mut buf: &[u8],
) -> usize {
    let mut scratch = [0u8; SECTOR_LEN];
    let mut matched = 0usize;
    while !buf.is_empty() {
        lfg.seek(id, disc, offset);
        let room = SECTOR_LEN - usize::try_from(offset % SECTOR_LEN as u64).expect("< sector");
        let n = room.min(buf.len());
        lfg.fill(&mut scratch[..n]);
        let run = buf[..n]
            .iter()
            .zip(&scratch[..n])
            .take_while(|(a, b)| a == b)
            .count();
        matched += run;
        if run != n {
            break;
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    matched
}

pub mod params;

#[cfg(target_arch = "wasm32")]
mod component;

#[cfg(test)]
mod tests;
