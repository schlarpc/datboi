//! MAME's bit reader and canonical Huffman decoder, as CHD uses them.
//!
//! Two CHD structures are coded with this pair: the v5 hunk map (a
//! 16-symbol tree exported in the "RLE" form) and the `huff` hunk codec
//! (a 256-symbol tree exported in the "Huffman" form — the code lengths
//! are themselves huffman-coded by a small 24-symbol tree). Both are
//! bit-exact reimplementations of `util/bitstream.h` and
//! `util/huffman.cpp`; the canonical-code assignment in particular is
//! MAME's own (longest length first, halving as it walks down), NOT the
//! DEFLATE convention, so it cannot be borrowed from a zlib-shaped
//! decoder.

use super::ChdError;

/// MSB-first bit reader over a byte slice. Reads past the end yield
/// zero bits and set [`BitReader::overflowed`] — MAME's behaviour, and
/// the reason a truncated map decodes to garbage that the map CRC then
/// rejects instead of panicking.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Next byte to pull in. May run past `data.len()`; see `overflowed`.
    pos: usize,
    /// Top `bits` bits of this word are live, MSB-aligned.
    buffer: u32,
    bits: u32,
}

impl<'a> BitReader<'a> {
    pub(crate) const fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            buffer: 0,
            bits: 0,
        }
    }

    /// Bits consumed so far, rounded up to a whole byte — how CHD's CD
    /// codecs find where one bit-coded stream ends and the next
    /// byte-aligned one begins.
    pub(crate) const fn byte_position(&self) -> usize {
        // `pos` counts bytes pulled into the buffer; `bits` of them are
        // still unconsumed.
        self.pos - (self.bits / 8) as usize
    }

    /// Whether the reader has *consumed* more than there was. `pos`
    /// alone runs ahead by design — a peek pulls four bytes whether or
    /// not they are wanted — so the question is asked of the consumed
    /// position, exactly as MAME asks it.
    pub(crate) const fn overflowed(&self) -> bool {
        self.byte_position() > self.data.len()
    }

    pub(crate) fn peek(&mut self, numbits: u32) -> u32 {
        if numbits == 0 {
            return 0;
        }
        if numbits > self.bits {
            while self.bits <= 24 {
                if self.pos < self.data.len() {
                    self.buffer |= u32::from(self.data[self.pos]) << (24 - self.bits);
                }
                self.pos += 1;
                self.bits += 8;
            }
        }
        self.buffer >> (32 - numbits)
    }

    pub(crate) const fn remove(&mut self, numbits: u32) {
        // A 32-bit shift is not defined for u32; a full-width read
        // simply empties the window.
        self.buffer = if numbits >= 32 {
            0
        } else {
            self.buffer << numbits
        };
        self.bits = self.bits.saturating_sub(numbits);
    }

    pub(crate) fn read(&mut self, numbits: u32) -> u32 {
        let v = self.peek(numbits);
        self.remove(numbits);
        v
    }

    /// Count the zero bits before the next one bit, consuming both —
    /// the unary quotient of a Rice code. Bounded so a corrupt stream
    /// ends the decode instead of spinning to the end of the file.
    pub(crate) fn read_unary(&mut self, limit: u32) -> Option<u32> {
        let mut n = 0;
        while self.read(1) == 0 {
            n += 1;
            if n > limit || self.overflowed() {
                return None;
            }
        }
        Some(n)
    }

    /// Discard bits up to the next byte boundary (FLAC frames are
    /// byte-aligned before their trailing CRC).
    pub(crate) fn align_to_byte(&mut self) {
        self.remove(self.bits % 8);
    }
}

/// A canonical Huffman decoder over `num_codes` symbols of at most
/// `max_bits` bits, driving a flat `1 << max_bits` lookup table exactly
/// as MAME does.
pub(crate) struct Huffman {
    num_codes: usize,
    max_bits: u32,
    /// Code length per symbol (0 = unused).
    lengths: Vec<u8>,
    /// Assigned code value per symbol.
    codes: Vec<u32>,
    /// `max_bits`-wide lookup: `(symbol << 5) | length`.
    lookup: Vec<u32>,
}

impl Huffman {
    fn new(num_codes: usize, max_bits: u32) -> Self {
        Self {
            num_codes,
            max_bits,
            lengths: vec![0; num_codes],
            codes: vec![0; num_codes],
            lookup: Vec::new(),
        }
    }

    /// MAME's `assign_canonical_codes`: walk lengths from longest to
    /// shortest, halving the running start at each step. A length
    /// histogram that does not halve cleanly is an invalid tree.
    fn assign_canonical_codes(&mut self) -> Result<(), ChdError> {
        let mut histo = [0u32; 33];
        for &len in &self.lengths {
            if u32::from(len) > self.max_bits {
                return Err(ChdError::Malformed(
                    "CHD huffman tree has an over-long code".into(),
                ));
            }
            histo[len as usize] += 1;
        }
        let mut curstart = 0u32;
        for codelen in (1..=32usize).rev() {
            let total = curstart + histo[codelen];
            let nextstart = total >> 1;
            if codelen != 1 && nextstart * 2 != total {
                return Err(ChdError::Malformed(
                    "CHD huffman tree is not a valid canonical code".into(),
                ));
            }
            histo[codelen] = curstart;
            curstart = nextstart;
        }
        for i in 0..self.num_codes {
            let len = self.lengths[i] as usize;
            if len > 0 {
                self.codes[i] = histo[len];
                histo[len] += 1;
            }
        }
        Ok(())
    }

    fn build_lookup_table(&mut self) {
        self.lookup = vec![0; 1usize << self.max_bits];
        for i in 0..self.num_codes {
            let len = u32::from(self.lengths[i]);
            if len == 0 {
                continue;
            }
            let value = (u32::try_from(i).expect("symbol index fits u32") << 5) | len;
            let shift = self.max_bits - len;
            let start = (self.codes[i] << shift) as usize;
            for slot in &mut self.lookup[start..start + (1usize << shift)] {
                *slot = value;
            }
        }
    }

    /// Decode one symbol. A bit pattern with no code assigned decodes
    /// to symbol 0 with length 0 — which consumes nothing and would
    /// spin, so callers guard with the reader's overflow flag and the
    /// structure's own checksum.
    pub(crate) fn decode_one(&self, bits: &mut BitReader<'_>) -> u32 {
        let peeked = bits.peek(self.max_bits) as usize;
        let entry = self.lookup[peeked];
        bits.remove(entry & 0x1f);
        entry >> 5
    }

    /// Build the decoder for a known length assignment — what a writer
    /// needs to emit codes the reader will agree with. There is exactly
    /// one canonical-code implementation on purpose: an encoder with
    /// its own copy could drift, and the format's own definition is
    /// whatever the decoder does.
    ///
    /// # Errors
    /// [`ChdError::Malformed`] if the lengths are not a valid canonical
    /// code.
    pub(crate) fn from_lengths(lengths: &[u8], max_bits: u32) -> Result<Self, ChdError> {
        let mut huff = Self::new(lengths.len(), max_bits);
        huff.lengths.copy_from_slice(lengths);
        huff.assign_canonical_codes()?;
        huff.build_lookup_table();
        Ok(huff)
    }

    /// Emit one symbol's code. Fixture writing only.
    pub(crate) fn encode_one(&self, bits: &mut BitWriter, symbol: usize) {
        bits.write(self.codes[symbol], u32::from(self.lengths[symbol]));
    }

    /// MAME's `import_tree_rle`: code lengths as raw `numbits`-wide
    /// values, where the value 1 escapes (`1,1` = a literal 1;
    /// `1,v,n` = value `v` repeated `n + 3` times). Used for the 16
    /// symbols of the v5 hunk map.
    pub(crate) fn import_tree_rle(
        num_codes: usize,
        max_bits: u32,
        bits: &mut BitReader<'_>,
    ) -> Result<Self, ChdError> {
        let mut huff = Self::new(num_codes, max_bits);
        let numbits = if max_bits >= 16 {
            5
        } else if max_bits >= 8 {
            4
        } else {
            3
        };
        let mut cur = 0usize;
        while cur < num_codes {
            let nodebits = bits.read(numbits);
            if nodebits != 1 {
                huff.lengths[cur] = u8::try_from(nodebits).expect("numbits <= 5");
                cur += 1;
                continue;
            }
            let nodebits = bits.read(numbits);
            if nodebits == 1 {
                huff.lengths[cur] = 1;
                cur += 1;
                continue;
            }
            let repcount = bits.read(numbits) + 3;
            for _ in 0..repcount {
                if cur >= num_codes {
                    return Err(ChdError::Malformed(
                        "CHD huffman RLE tree overruns its symbol count".into(),
                    ));
                }
                huff.lengths[cur] = u8::try_from(nodebits).expect("numbits <= 5");
                cur += 1;
            }
        }
        if bits.overflowed() {
            return Err(ChdError::Malformed("CHD huffman tree is truncated".into()));
        }
        huff.assign_canonical_codes()?;
        huff.build_lookup_table();
        Ok(huff)
    }

    /// MAME's `import_tree_huffman`: the code lengths are themselves
    /// coded by a 24-symbol / 6-bit tree whose own lengths are 3-bit
    /// raw values. Used for the 256 symbols of the `huff` hunk codec.
    pub(crate) fn import_tree_huffman(
        num_codes: usize,
        max_bits: u32,
        bits: &mut BitReader<'_>,
    ) -> Result<Self, ChdError> {
        // The small tree's own lengths: index 0 raw, then a start
        // index, then 3-bit lengths — where a literal 7 both means
        // "unused" and terminates the run (MAME's `count == 7` latch).
        let mut small = Self::new(24, 6);
        small.lengths[0] = u8::try_from(bits.read(3)).expect("3 bits");
        let start = bits.read(3) as usize + 1;
        let mut count = 0u32;
        for index in 1..24 {
            if index < start || count == 7 {
                small.lengths[index] = 0;
            } else {
                count = bits.read(3);
                small.lengths[index] = if count == 7 {
                    0
                } else {
                    u8::try_from(count).expect("3 bits")
                };
            }
        }
        small.assign_canonical_codes()?;
        small.build_lookup_table();

        // Widest RLE count the symbol space can need.
        let mut temp = num_codes - 9;
        let mut rlefullbits = 0u32;
        while temp != 0 {
            temp >>= 1;
            rlefullbits += 1;
        }

        let mut huff = Self::new(num_codes, max_bits);
        let mut last = 0u8;
        let mut cur = 0usize;
        while cur < num_codes {
            let value = small.decode_one(bits);
            if value != 0 {
                last = u8::try_from(value - 1).expect("small tree symbol < 24");
                huff.lengths[cur] = last;
                cur += 1;
            } else {
                let mut count = bits.read(3) + 2;
                if count == 7 + 2 {
                    count += bits.read(rlefullbits);
                }
                while count != 0 && cur < num_codes {
                    huff.lengths[cur] = last;
                    cur += 1;
                    count -= 1;
                }
            }
            if bits.overflowed() {
                return Err(ChdError::Malformed("CHD huffman tree is truncated".into()));
            }
        }
        huff.assign_canonical_codes()?;
        huff.build_lookup_table();
        Ok(huff)
    }
}

/// CRC-16/CCITT-FALSE (poly 0x1021, init 0xffff) — what a v5 map
/// header pins its decompressed map with, and what each compressed v5
/// hunk entry carries.
#[must_use]
pub(crate) fn crc16(data: &[u8]) -> u16 {
    let mut acc: u16 = 0xffff;
    for &b in data {
        let idx = ((acc >> 8) ^ u16::from(b)) & 0xff;
        acc = (acc << 8) ^ crc16_table()[idx as usize];
    }
    acc
}

fn crc16_table() -> &'static [u16; 256] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u16; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u16; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut crc = u16::try_from(i).expect("index < 256") << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 {
                    (crc << 1) ^ 0x1021
                } else {
                    crc << 1
                };
            }
            *slot = crc;
        }
        t
    })
}

/// MSB-first bit writer — the mirror of [`BitReader`]. Only fixture
/// synthesis writes CHD structures, so this lives beside the reader it
/// has to agree with rather than in the synth module.
pub(crate) struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    bits: u32,
}

impl BitWriter {
    pub(crate) const fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            bits: 0,
        }
    }

    pub(crate) fn write(&mut self, value: u32, numbits: u32) {
        for i in (0..numbits).rev() {
            let bit = (value >> i) & 1;
            self.acc = (self.acc << 1) | bit;
            self.bits += 1;
            if self.bits == 8 {
                self.out
                    .push(u8::try_from(self.acc & 0xff).expect("masked"));
                self.acc = 0;
                self.bits = 0;
            }
        }
    }

    /// Pad with zero bits to the next byte boundary.
    pub(crate) fn align_to_byte(&mut self) {
        if self.bits > 0 {
            self.write(0, 8 - self.bits);
        }
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.align_to_byte();
        self.out
    }

    /// The bytes written so far, whole bytes only.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_reader_is_msb_first_and_flags_overflow() {
        let mut r = BitReader::new(&[0b1011_0010, 0xff]);
        assert_eq!(r.read(1), 1);
        assert_eq!(r.read(3), 0b011);
        assert_eq!(r.read(4), 0b0010);
        assert_eq!(r.read(8), 0xff);
        assert!(!r.overflowed());
        let _ = r.read(8);
        assert!(r.overflowed());
    }

    #[test]
    fn byte_position_tracks_whole_bytes_consumed() {
        let mut r = BitReader::new(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(r.byte_position(), 0);
        r.read(8);
        assert_eq!(r.byte_position(), 1);
        r.read(16);
        assert_eq!(r.byte_position(), 3);
    }

    /// The v5 map's tree form: three symbols, one of them RLE-repeated.
    #[test]
    fn rle_tree_round_trips_a_hand_built_stream() {
        let mut w = BitWriter::new();
        // Lengths for 16 symbols: 1, 1, then 14 zeroes via the escape.
        w.write(1, 4); // escape
        w.write(1, 4); // ...doubled: a literal length of 1
        w.write(1, 4); // escape
        w.write(1, 4); // ...doubled: a literal length of 1
        w.write(1, 4); // escape
        w.write(0, 4); // repeated value: 0
        w.write(14 - 3, 4); // repeated 14 times
        // Symbols: canonical codes are 0 and 1, one bit each.
        w.write(0, 1);
        w.write(1, 1);
        w.write(1, 1);
        w.write(0, 1);
        let bytes = w.finish();

        let mut r = BitReader::new(&bytes);
        let huff = Huffman::import_tree_rle(16, 8, &mut r).expect("valid tree");
        assert_eq!(
            [
                huff.decode_one(&mut r),
                huff.decode_one(&mut r),
                huff.decode_one(&mut r),
                huff.decode_one(&mut r),
            ],
            [0, 1, 1, 0]
        );
    }

    #[test]
    fn crc16_matches_the_ccitt_false_vector() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }
}
