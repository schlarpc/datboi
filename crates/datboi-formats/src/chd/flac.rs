//! The FLAC subset CHD's `flac` / `cdfl` codecs write.
//!
//! chdman does not store a FLAC *stream*: it stores bare FLAC frames,
//! with no `fLaC` marker and no STREAMINFO block (MAME synthesises one
//! to feed libFLAC). Every frame carries its own block size, rate,
//! channel assignment and sample depth, so a frame-level decoder needs
//! none of that — which is also what lets `cdfl` find where the FLAC
//! data stops and the deflated subcode run begins, something a
//! whole-stream decoder cannot report.
//!
//! Only what chdman emits is accepted: 16-bit samples, 1–8 channels
//! (CHD always writes 2). Anything else is a refusal with a reason,
//! never a partial decode.

use super::ChdError;
use super::huffman::BitReader;

/// Bytes of PCM per stereo 16-bit frame sample.
const BYTES_PER_SAMPLE: usize = 4;

/// Decode the plain `flac` hunk codec. Its first byte is an endianness
/// marker ('L'/'B') naming the byte order the samples were taken in;
/// the rest is bare FLAC frames.
pub(crate) fn decode_hunk(src: &[u8], dest: &mut [u8]) -> Result<(), ChdError> {
    let (&marker, frames) = src
        .split_first()
        .ok_or_else(|| ChdError::Malformed("flac hunk is empty".into()))?;
    let big_endian = match marker {
        b'B' => true,
        b'L' => false,
        other => {
            return Err(ChdError::Malformed(format!(
                "flac hunk endianness marker is 0x{other:02x}, not 'L' or 'B'"
            )));
        }
    };
    decode_frames(frames, dest, big_endian).map(|_| ())
}

/// Decode the sector run of a `cdfl` hunk, answering how many bytes of
/// `src` the FLAC frames consumed — the deflated subcode run starts
/// there. CD audio in a CHD is big-endian whatever the host is.
pub(crate) fn decode_cd_sectors(src: &[u8], dest: &mut [u8]) -> Result<usize, ChdError> {
    decode_frames(src, dest, true)
}

/// Decode frames until `dest` is full. Returns the number of source
/// bytes consumed.
fn decode_frames(src: &[u8], dest: &mut [u8], big_endian: bool) -> Result<usize, ChdError> {
    if !dest.len().is_multiple_of(BYTES_PER_SAMPLE) {
        return Err(ChdError::Malformed(format!(
            "flac hunk output of {} bytes is not whole stereo samples",
            dest.len()
        )));
    }
    let mut bits = BitReader::new(src);
    let mut written = 0usize;
    let mut channels: Vec<Vec<i32>> = Vec::new();
    while written < dest.len() {
        let frame_start = bits.byte_position();
        let header = read_frame_header(&mut bits)?;
        decode_subframes(&mut bits, &header, &mut channels)?;
        // Frames end byte-aligned with a CRC-16 over everything from
        // the sync code; a mismatch is corruption, which is exactly
        // what a verify exists to find.
        bits.align_to_byte();
        let payload_end = bits.byte_position();
        let stored = u16::from_be_bytes(
            src.get(payload_end..payload_end + 2)
                .ok_or_else(|| ChdError::Malformed("flac frame is truncated".into()))?
                .try_into()
                .expect("2 bytes"),
        );
        let want = crc16_flac(&src[frame_start..payload_end]);
        if stored != want {
            return Err(ChdError::Malformed(format!(
                "flac frame CRC-16 is {stored:#06x}, computed {want:#06x}"
            )));
        }
        bits.read(16);

        let want_bytes = header.block_size * header.channels * 2;
        if written + want_bytes > dest.len() {
            return Err(ChdError::Malformed(
                "flac frames decode past the hunk's length".into(),
            ));
        }
        let out = &mut dest[written..written + want_bytes];
        let stride = header.channels * 2;
        for (ch, buf) in channels.iter().take(header.channels).enumerate() {
            for (sample, &value) in buf.iter().enumerate() {
                let value = i16::try_from(value)
                    .map_err(|_| ChdError::Malformed("flac sample does not fit 16 bits".into()))?;
                let at = sample * stride + ch * 2;
                let bytes = if big_endian {
                    value.to_be_bytes()
                } else {
                    value.to_le_bytes()
                };
                out[at..at + 2].copy_from_slice(&bytes);
            }
        }
        written += want_bytes;
        if bits.overflowed() {
            return Err(ChdError::Malformed("flac data is truncated".into()));
        }
    }
    Ok(bits.byte_position())
}

struct FrameHeader {
    block_size: usize,
    channels: usize,
    /// 0–7 independent, 8 left/side, 9 right/side, 10 mid/side.
    assignment: u32,
    bits_per_sample: u32,
}

fn read_frame_header(bits: &mut BitReader<'_>) -> Result<FrameHeader, ChdError> {
    if bits.read(14) != 0x3ffe {
        return Err(ChdError::Malformed(
            "flac frame sync code is missing".into(),
        ));
    }
    if bits.read(1) != 0 {
        return Err(ChdError::Malformed("flac frame reserved bit is set".into()));
    }
    let _blocking_strategy = bits.read(1);
    let bs_code = bits.read(4);
    let sr_code = bits.read(4);
    let assignment = bits.read(4);
    let bps_code = bits.read(3);
    if bits.read(1) != 0 {
        return Err(ChdError::Malformed(
            "flac frame header reserved bit is set".into(),
        ));
    }
    // The coded frame/sample number, UTF-8 style: the leading byte's
    // high bits give the length.
    let lead = bits.read(8);
    let extra = match lead {
        0x00..=0x7f => 0,
        0xc0..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf7 => 3,
        0xf8..=0xfb => 4,
        0xfc..=0xfd => 5,
        0xfe => 6,
        _ => {
            return Err(ChdError::Malformed(
                "flac frame number is not valid UTF-8 coding".into(),
            ));
        }
    };
    for _ in 0..extra {
        if bits.read(8) & 0xc0 != 0x80 {
            return Err(ChdError::Malformed(
                "flac frame number continuation byte is malformed".into(),
            ));
        }
    }

    let block_size = match bs_code {
        0 => {
            return Err(ChdError::Malformed(
                "flac block size code 0 is reserved".into(),
            ));
        }
        1 => 192,
        2..=5 => 576 << (bs_code - 2),
        6 => bits.read(8) as usize + 1,
        7 => bits.read(16) as usize + 1,
        _ => 256 << (bs_code - 8),
    };
    match sr_code {
        12 => {
            bits.read(8);
        }
        13 | 14 => {
            bits.read(16);
        }
        15 => {
            return Err(ChdError::Malformed(
                "flac sample rate code 15 is invalid".into(),
            ));
        }
        _ => {}
    }
    let bits_per_sample = match bps_code {
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => {
            return Err(ChdError::Unsupported(
                "flac frame declares its sample depth in STREAMINFO, which CHD does not store"
                    .into(),
            ));
        }
    };
    if bits_per_sample != 16 {
        return Err(ChdError::Unsupported(format!(
            "flac hunk is {bits_per_sample}-bit; CHD only ever writes 16"
        )));
    }
    let channels = match assignment {
        0..=7 => assignment as usize + 1,
        8..=10 => 2,
        _ => {
            return Err(ChdError::Malformed(
                "flac channel assignment is reserved".into(),
            ));
        }
    };

    // The header's own CRC-8 trailer. Not checked separately: the
    // frame's CRC-16 covers these same bytes, and a decoder that
    // reached this point already agreed with every field.
    bits.read(8);
    Ok(FrameHeader {
        block_size,
        channels,
        assignment,
        bits_per_sample,
    })
}

fn decode_subframes(
    bits: &mut BitReader<'_>,
    header: &FrameHeader,
    channels: &mut Vec<Vec<i32>>,
) -> Result<(), ChdError> {
    channels.resize_with(header.channels, Vec::new);
    for buf in channels.iter_mut() {
        buf.clear();
        buf.resize(header.block_size, 0);
    }
    for ch in 0..header.channels {
        // Side channels carry one extra bit of range.
        let extra = u32::from(match header.assignment {
            8 | 10 if ch == 1 => true,
            9 if ch == 0 => true,
            _ => false,
        });
        decode_subframe(
            bits,
            header.block_size,
            header.bits_per_sample + extra,
            &mut channels[ch],
        )?;
    }
    // Undo the inter-channel decorrelation. One channel always holds
    // the *difference*; which one, and how the pair recombines, is what
    // the assignment says.
    if matches!(header.assignment, 8..=10) {
        let (first, rest) = channels.split_at_mut(1);
        let (left, right) = (&mut first[0], &mut rest[0]);
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            match header.assignment {
                // left + side: the second channel is left - right.
                8 => *r = *l - *r,
                // right + side: the first channel is left - right.
                9 => *l += *r,
                // mid + side, with the mid's low bit carried in the side.
                _ => {
                    let side = *r;
                    let mid = (*l << 1) | (side & 1);
                    *l = (mid + side) >> 1;
                    *r = (mid - side) >> 1;
                }
            }
        }
    }
    Ok(())
}

fn decode_subframe(
    bits: &mut BitReader<'_>,
    block_size: usize,
    bps: u32,
    out: &mut [i32],
) -> Result<(), ChdError> {
    if bits.read(1) != 0 {
        return Err(ChdError::Malformed(
            "flac subframe padding bit is set".into(),
        ));
    }
    let kind = bits.read(6);
    let wasted = if bits.read(1) == 1 {
        bits.read_unary(64)
            .ok_or_else(|| ChdError::Malformed("flac wasted-bits count runs away".into()))?
            + 1
    } else {
        0
    };
    let bps = bps
        .checked_sub(wasted)
        .filter(|b| *b > 0 && *b <= 32)
        .ok_or_else(|| ChdError::Malformed("flac subframe wastes every bit".into()))?;

    match kind {
        0 => {
            let v = sign_extend(bits.read(bps), bps);
            out.fill(v);
        }
        1 => {
            for slot in out.iter_mut() {
                *slot = sign_extend(bits.read(bps), bps);
            }
        }
        8..=12 => {
            let order = (kind - 8) as usize;
            for slot in out.iter_mut().take(order) {
                *slot = sign_extend(bits.read(bps), bps);
            }
            decode_residual(bits, block_size, order, out)?;
            fixed_predict(order, out);
        }
        32..=63 => {
            let order = (kind - 31) as usize;
            for slot in out.iter_mut().take(order) {
                *slot = sign_extend(bits.read(bps), bps);
            }
            let precision = bits.read(4) + 1;
            if precision == 16 {
                return Err(ChdError::Malformed(
                    "flac LPC coefficient precision is the reserved value".into(),
                ));
            }
            let shift = sign_extend(bits.read(5), 5);
            if shift < 0 {
                return Err(ChdError::Malformed(
                    "flac LPC shift is negative, which no encoder emits".into(),
                ));
            }
            let coeffs: Vec<i32> = (0..order)
                .map(|_| sign_extend(bits.read(precision), precision))
                .collect();
            decode_residual(bits, block_size, order, out)?;
            lpc_predict(&coeffs, shift as u32, order, out);
        }
        other => {
            return Err(ChdError::Malformed(format!(
                "flac subframe type {other} is reserved"
            )));
        }
    }
    if wasted > 0 {
        for slot in out.iter_mut() {
            *slot <<= wasted;
        }
    }
    Ok(())
}

fn decode_residual(
    bits: &mut BitReader<'_>,
    block_size: usize,
    order: usize,
    out: &mut [i32],
) -> Result<(), ChdError> {
    let method = bits.read(2);
    let (param_bits, escape) = match method {
        0 => (4u32, 15u32),
        1 => (5, 31),
        _ => {
            return Err(ChdError::Malformed(
                "flac residual coding method is reserved".into(),
            ));
        }
    };
    let partition_order = bits.read(4);
    let partitions = 1usize << partition_order;
    if !block_size.is_multiple_of(partitions) || block_size >> partition_order < order {
        return Err(ChdError::Malformed(
            "flac residual partition order does not divide the block".into(),
        ));
    }
    let mut at = order;
    for p in 0..partitions {
        let count = (block_size >> partition_order) - if p == 0 { order } else { 0 };
        let param = bits.read(param_bits);
        if param == escape {
            let raw = bits.read(5);
            for slot in out.iter_mut().skip(at).take(count) {
                *slot = if raw == 0 {
                    0
                } else {
                    sign_extend(bits.read(raw), raw)
                };
            }
        } else {
            for slot in out.iter_mut().skip(at).take(count) {
                let quotient = bits
                    .read_unary(1 << 20)
                    .ok_or_else(|| ChdError::Malformed("flac Rice quotient runs away".into()))?;
                let remainder = bits.read(param);
                let value = (quotient << param) | remainder;
                // Zigzag: LSB is the sign.
                #[allow(clippy::cast_possible_wrap)]
                let signed = ((value >> 1) as i32) ^ -((value & 1) as i32);
                *slot = signed;
            }
        }
        at += count;
        if bits.overflowed() {
            return Err(ChdError::Malformed("flac residual is truncated".into()));
        }
    }
    Ok(())
}

/// FLAC's fixed polynomial predictors, applied in place over the
/// residuals already in `out`.
fn fixed_predict(order: usize, out: &mut [i32]) {
    for i in order..out.len() {
        let p = match order {
            0 => 0,
            1 => i64::from(out[i - 1]),
            2 => 2 * i64::from(out[i - 1]) - i64::from(out[i - 2]),
            3 => 3 * i64::from(out[i - 1]) - 3 * i64::from(out[i - 2]) + i64::from(out[i - 3]),
            _ => {
                4 * i64::from(out[i - 1]) - 6 * i64::from(out[i - 2]) + 4 * i64::from(out[i - 3])
                    - i64::from(out[i - 4])
            }
        };
        out[i] = i32::try_from(i64::from(out[i]) + p).unwrap_or(i32::MAX);
    }
}

fn lpc_predict(coeffs: &[i32], shift: u32, order: usize, out: &mut [i32]) {
    for i in order..out.len() {
        let mut acc = 0i64;
        for (j, &c) in coeffs.iter().enumerate() {
            acc += i64::from(c) * i64::from(out[i - 1 - j]);
        }
        out[i] = i32::try_from(i64::from(out[i]) + (acc >> shift)).unwrap_or(i32::MAX);
    }
}

#[allow(clippy::cast_possible_wrap)]
const fn sign_extend(value: u32, bits: u32) -> i32 {
    if bits == 0 {
        return 0;
    }
    if bits >= 32 {
        return value as i32;
    }
    let shift = 32 - bits;
    ((value << shift) as i32) >> shift
}

/// CRC-16 with poly 0x8005 and a zero seed — FLAC's frame footer.
fn crc16_flac(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}
