//! `xf-cso`: CSO v1 ("CISO") codec for the frozen `datboi:transform@2`
//! world — the second real component, chosen to exercise every contract
//! surface the reference guests don't:
//!
//! * **CBOR params** (`compress` takes block-size/level/align);
//! * **per-op seek classes** (`compress` is opaque, `decompress` is
//!   manifest-seekable — the index IS the manifest);
//! * **`random-access-inputs`** in the descriptor (`compress` declares
//!   input 0 random-access and reads it twice: sizing pass, then write
//!   pass — CSO's index precedes its blocks and sinks can't seek);
//! * **in-guest compression** (miniz_oxide, exact-pinned: the component
//!   hash pins the compressor, so `compress` output is a valid recipe
//!   claim — D5 by construction).
//!
//! Ops:
//! * `compress`: ISO → CSO. Params: canonical CBOR map
//!   `{1: block-size, 2: level, 3: align}` (uint keys/values; empty
//!   params = `{2048, 9, 0}`).
//! * `decompress`: CSO → ISO. No params. `serve-range` inflates only the
//!   blocks a window touches.
//!
//! Format (v1, matching ciso.c lineage): 24-byte header
//! `"CISO" | header_size u32 | total u64 | block_size u32 | ver=1 | align
//! | pad[2]`, then `n+1` u32 index entries (`bit31` = stored-raw, low 31
//! bits = file offset `>> align`), then blocks. A compressed block's
//! extent runs to the next entry's offset; stored blocks hold the block's
//! plaintext verbatim.

/// Parsed/validated CSO header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub total: u64,
    pub block_size: u32,
    pub align: u8,
}

pub const HEADER_LEN: usize = 24;
pub const STORED_FLAG: u32 = 0x8000_0000;
pub const OFFSET_MASK: u32 = 0x7FFF_FFFF;

/// Compression parameters for `compress` (op-owned CBOR schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressParams {
    pub block_size: u32,
    pub level: u8,
    pub align: u8,
}

impl Default for CompressParams {
    fn default() -> Self {
        Self {
            block_size: 2048,
            level: 9,
            align: 0,
        }
    }
}

const PARAM_BLOCK_SIZE: u64 = 1;
const PARAM_LEVEL: u64 = 2;
const PARAM_ALIGN: u64 = 3;

/// Decode the canonical-CBOR uint-keyed map this op owns. Empty bytes are
/// the defaults. Anything else — wrong types, unknown keys, non-canonical
/// order — is a deterministic error: params are recipe content, and two
/// encodings of "the same" params must not both work.
///
/// # Errors
/// On malformed CBOR or out-of-range values.
pub fn decode_params(bytes: &[u8]) -> Result<CompressParams, String> {
    if bytes.is_empty() {
        return Ok(CompressParams::default());
    }
    let mut p = CompressParams::default();
    let mut cursor = 0usize;
    let (n_pairs, head) = cbor_uint_header(bytes, &mut cursor, 0xa0, "map")?;
    if head > 23 {
        return Err("params map too large".into());
    }
    let mut last_key = None;
    for _ in 0..n_pairs {
        let (key, _) = cbor_uint_header(bytes, &mut cursor, 0x00, "uint key")?;
        if last_key.is_some_and(|k| key <= k) {
            return Err("params map keys not in canonical order".into());
        }
        last_key = Some(key);
        let (value, _) = cbor_uint_header(bytes, &mut cursor, 0x00, "uint value")?;
        match key {
            PARAM_BLOCK_SIZE => {
                if !(512..=1 << 20).contains(&value) || !value.is_power_of_two() {
                    return Err(format!("block-size {value} not a power of two in 512..=1MiB"));
                }
                p.block_size = u32::try_from(value).expect("bounded");
            }
            PARAM_LEVEL => {
                if !(1..=10).contains(&value) {
                    return Err(format!("level {value} outside 1..=10"));
                }
                p.level = u8::try_from(value).expect("bounded");
            }
            PARAM_ALIGN => {
                if value > 15 {
                    return Err(format!("align {value} outside 0..=15"));
                }
                p.align = u8::try_from(value).expect("bounded");
            }
            other => return Err(format!("unknown param key {other}")),
        }
    }
    if cursor != bytes.len() {
        return Err("trailing bytes after params map".into());
    }
    Ok(p)
}

/// Read one CBOR uint-shaped item (major type from `major_base`),
/// returning (value, additional-info). Canonical: shortest encoding only.
fn cbor_uint_header(
    bytes: &[u8],
    cursor: &mut usize,
    major_base: u8,
    what: &str,
) -> Result<(u64, u8), String> {
    let first = *bytes.get(*cursor).ok_or(format!("truncated {what}"))?;
    if first & 0xe0 != major_base {
        return Err(format!("expected {what}, got initial byte {first:#04x}"));
    }
    *cursor += 1;
    let info = first & 0x1f;
    let (value, extra) = match info {
        0..=23 => (u64::from(info), 0usize),
        24 => (
            u64::from(*bytes.get(*cursor).ok_or(format!("truncated {what}"))?),
            1,
        ),
        25 => {
            let b = bytes
                .get(*cursor..*cursor + 2)
                .ok_or(format!("truncated {what}"))?;
            (u64::from(u16::from_be_bytes(b.try_into().expect("2"))), 2)
        }
        26 => {
            let b = bytes
                .get(*cursor..*cursor + 4)
                .ok_or(format!("truncated {what}"))?;
            (u64::from(u32::from_be_bytes(b.try_into().expect("4"))), 4)
        }
        27 => {
            let b = bytes
                .get(*cursor..*cursor + 8)
                .ok_or(format!("truncated {what}"))?;
            (u64::from_be_bytes(b.try_into().expect("8")), 8)
        }
        _ => return Err(format!("unsupported {what} encoding {info}")),
    };
    *cursor += extra;
    let canonical = match value {
        0..=23 => 0,
        24..=0xff => 1,
        0x100..=0xffff => 2,
        0x1_0000..=0xffff_ffff => 4,
        _ => 8,
    };
    if extra != canonical {
        return Err(format!("non-canonical {what} encoding"));
    }
    Ok((value, info))
}

/// Encode the header (the split side / test counterpart).
#[must_use]
pub fn encode_header(h: Header) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[..4].copy_from_slice(b"CISO");
    out[4..8].copy_from_slice(&u32::try_from(HEADER_LEN).expect("24").to_le_bytes());
    out[8..16].copy_from_slice(&h.total.to_le_bytes());
    out[16..20].copy_from_slice(&h.block_size.to_le_bytes());
    out[20] = 1;
    out[21] = h.align;
    out
}

/// Parse and validate a CSO header.
///
/// # Errors
/// On a wrong magic/version or nonsense geometry.
pub fn parse_header(bytes: &[u8]) -> Result<Header, String> {
    let Ok(fixed): Result<&[u8; HEADER_LEN], _> = bytes.try_into() else {
        return Err(format!("header is {} of {HEADER_LEN} bytes", bytes.len()));
    };
    if &fixed[..4] != b"CISO" {
        return Err("bad magic: not a CSO v1 container".into());
    }
    if fixed[20] != 1 {
        return Err(format!("unsupported CSO version {}", fixed[20]));
    }
    let block_size = u32::from_le_bytes(fixed[16..20].try_into().expect("4"));
    if block_size == 0 || !block_size.is_power_of_two() {
        return Err(format!("block size {block_size} not a power of two"));
    }
    Ok(Header {
        total: u64::from_le_bytes(fixed[8..16].try_into().expect("8")),
        block_size,
        align: fixed[21],
    })
}

/// Blocks `[first, last]` (inclusive) touched by ISO window
/// `[offset, offset+len)`, clamped to the file; `None` when the clamped
/// window is empty.
#[must_use]
pub fn touched_blocks(total: u64, block_size: u32, offset: u64, len: u64) -> Option<(u64, u64)> {
    let bs = u64::from(block_size);
    let start = offset.min(total);
    let end = offset.saturating_add(len).min(total);
    if start >= end {
        return None;
    }
    Some((start / bs, (end - 1) / bs))
}

/// Component glue for the frozen `datboi:transform@2` world; wasm32-only
/// so host-side tests of the pure logic build natively.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod component {
    wit_bindgen::generate!({
        world: "transform-stream",
        path: "../wit/v2",
    });

    use datboi::transform::types::{File, SeekClass, Source};

    use super::{
        CompressParams, HEADER_LEN, Header, OFFSET_MASK, STORED_FLAG, decode_params,
        encode_header, parse_header, touched_blocks,
    };

    struct Xf;

    fn expect_sequential(input: &Input) -> Result<&Source, String> {
        match input {
            Input::Sequential(s) => Ok(s),
            Input::RandomAccess(_) => Err("expected a sequential input".into()),
        }
    }

    fn expect_random(input: &Input) -> Result<&File, String> {
        match input {
            Input::RandomAccess(f) => Ok(f),
            Input::Sequential(_) => Err("expected a random-access input".into()),
        }
    }

    fn read_exact_at(file: &File, offset: u64, n: u32, what: &str) -> Result<Vec<u8>, String> {
        let bytes = file.read_at(offset, n);
        if bytes.len() as u64 != u64::from(n) {
            return Err(format!(
                "{what}: wanted {n} bytes at {offset}, got {}",
                bytes.len()
            ));
        }
        Ok(bytes)
    }

    fn read_exact_seq(src: &Source, n: u64, what: &str) -> Result<Vec<u8>, String> {
        let n32 = u32::try_from(n).map_err(|_| format!("{what}: read of {n} bytes"))?;
        let bytes = src.read(n32);
        if bytes.len() as u64 != n {
            return Err(format!("{what}: wanted {n} bytes, stream ended early"));
        }
        Ok(bytes)
    }

    /// One block's compressed form: bytes to write plus the stored flag.
    fn deflate_block(plain: &[u8], level: u8) -> (Vec<u8>, bool) {
        let compressed = miniz_oxide::deflate::compress_to_vec(plain, level);
        if compressed.len() >= plain.len() {
            (plain.to_vec(), true)
        } else {
            (compressed, false)
        }
    }

    /// Sizing pass + write pass share this iteration: yields each block's
    /// plaintext length in order.
    fn block_lens(total: u64, bs: u32) -> impl Iterator<Item = u64> {
        let bs = u64::from(bs);
        let n = total.div_ceil(bs);
        (0..n).map(move |i| (total - i * bs).min(bs))
    }

    fn compress(params: &CompressParams, iso: &File, out: &Sink) -> Result<(), String> {
        let total = iso.len();
        let bs = params.block_size;
        let align = u32::from(params.align);
        let n_blocks = total.div_ceil(u64::from(bs));
        usize::try_from(n_blocks.checked_add(1).ok_or("block count overflow")?)
            .map_err(|_| "block count exceeds memory")?;

        // Pass 1: compress every block for its size; build the index.
        let mut index: Vec<u32> = Vec::with_capacity(usize::try_from(n_blocks + 1).expect("checked"));
        let mut cursor: u64 = HEADER_LEN as u64 + (n_blocks + 1) * 4;
        let mut pos: u64 = 0;
        for plain_len in block_lens(total, bs) {
            let pad = cursor.next_multiple_of(1 << align) - cursor;
            cursor += pad;
            let entry_off = cursor >> align;
            if entry_off > u64::from(OFFSET_MASK) {
                return Err("output exceeds CSO v1 31-bit offset space; raise align".into());
            }
            let plain = read_exact_at(iso, pos, u32::try_from(plain_len).expect("<= bs"), "iso block")?;
            pos += plain_len;
            let (bytes, stored) = deflate_block(&plain, params.level);
            let mut entry = u32::try_from(entry_off).expect("checked");
            if stored {
                entry |= STORED_FLAG;
            }
            index.push(entry);
            cursor += bytes.len() as u64;
        }
        let end_off = cursor.next_multiple_of(1 << align) >> align;
        if end_off > u64::from(OFFSET_MASK) {
            return Err("output exceeds CSO v1 31-bit offset space; raise align".into());
        }
        index.push(u32::try_from(end_off).expect("checked"));

        // Write pass: header, index, then re-compress each block.
        out.write(&encode_header(Header {
            total,
            block_size: bs,
            align: params.align,
        }));
        let mut index_bytes = Vec::with_capacity(index.len() * 4);
        for entry in &index {
            index_bytes.extend_from_slice(&entry.to_le_bytes());
        }
        out.write(&index_bytes);
        let mut cursor: u64 = HEADER_LEN as u64 + (n_blocks + 1) * 4;
        let mut pos: u64 = 0;
        for plain_len in block_lens(total, bs) {
            let pad = cursor.next_multiple_of(1 << align) - cursor;
            if pad > 0 {
                out.write(&vec![0u8; usize::try_from(pad).expect("small")]);
                cursor += pad;
            }
            let plain = read_exact_at(iso, pos, u32::try_from(plain_len).expect("<= bs"), "iso block")?;
            pos += plain_len;
            let (bytes, _) = deflate_block(&plain, params.level);
            out.write(&bytes);
            cursor += bytes.len() as u64;
        }
        Ok(())
    }

    /// Decode one block extent (bytes between its offset and the next
    /// entry's) into plaintext of exactly `plain_len` bytes.
    fn decode_block(
        extent: &[u8],
        stored: bool,
        plain_len: u64,
        block_ix: u64,
    ) -> Result<Vec<u8>, String> {
        if stored {
            let plain_len = usize::try_from(plain_len).expect("<= bs");
            if extent.len() < plain_len {
                return Err(format!("stored block {block_ix} shorter than plaintext"));
            }
            Ok(extent[..plain_len].to_vec())
        } else {
            let plain = miniz_oxide::inflate::decompress_to_vec(extent)
                .map_err(|e| format!("block {block_ix} does not inflate: {e:?}"))?;
            if plain.len() as u64 != plain_len {
                return Err(format!(
                    "block {block_ix} inflated to {} of {plain_len} bytes",
                    plain.len()
                ));
            }
            Ok(plain)
        }
    }

    fn decompress(cso: &Source, out: &Sink) -> Result<(), String> {
        let header = parse_header(&read_exact_seq(cso, HEADER_LEN as u64, "header")?)?;
        let n_blocks = header.total.div_ceil(u64::from(header.block_size));
        let index_bytes = read_exact_seq(cso, (n_blocks + 1) * 4, "index")?;
        let index: Vec<u32> = index_bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().expect("4")))
            .collect();
        let mut file_pos = HEADER_LEN as u64 + (n_blocks + 1) * 4;
        for (i, plain_len) in block_lens(header.total, header.block_size).enumerate() {
            let entry = index[i];
            let data_off = u64::from(entry & OFFSET_MASK) << header.align;
            let next_off = u64::from(index[i + 1] & OFFSET_MASK) << header.align;
            if data_off < file_pos || next_off < data_off {
                return Err(format!("index entry {i} goes backwards"));
            }
            // Skip alignment padding.
            if data_off > file_pos {
                read_exact_seq(cso, data_off - file_pos, "padding")?;
            }
            let extent = read_exact_seq(cso, next_off - data_off, "block extent")?;
            file_pos = next_off;
            let plain = decode_block(&extent, entry & STORED_FLAG != 0, plain_len, i as u64)?;
            out.write(&plain);
        }
        Ok(())
    }

    fn serve_decompressed_range(
        cso: &File,
        offset: u64,
        len: u64,
        out: &Sink,
    ) -> Result<(), String> {
        let header = parse_header(&read_exact_at(cso, 0, HEADER_LEN as u32, "header")?)?;
        let bs = u64::from(header.block_size);
        let Some((first, last)) = touched_blocks(header.total, header.block_size, offset, len)
        else {
            return Ok(()); // empty clamped window
        };
        // Index slice for blocks first..=last plus the sentinel.
        let n_entries = last - first + 2;
        let index_bytes = read_exact_at(
            cso,
            HEADER_LEN as u64 + first * 4,
            u32::try_from(n_entries * 4).map_err(|_| "index slice too large")?,
            "index slice",
        )?;
        let index: Vec<u32> = index_bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().expect("4")))
            .collect();
        let win_start = offset.min(header.total);
        let win_end = offset.saturating_add(len).min(header.total);
        for block in first..=last {
            let i = usize::try_from(block - first).expect("bounded");
            let entry = index[i];
            let data_off = u64::from(entry & OFFSET_MASK) << header.align;
            let next_off = u64::from(index[i + 1] & OFFSET_MASK) << header.align;
            if next_off < data_off {
                return Err(format!("index entry {block} goes backwards"));
            }
            let extent = read_exact_at(
                cso,
                data_off,
                u32::try_from(next_off - data_off).map_err(|_| "block extent too large")?,
                "block extent",
            )?;
            let plain_len = (header.total - block * bs).min(bs);
            let plain = decode_block(&extent, entry & STORED_FLAG != 0, plain_len, block)?;
            let block_start = block * bs;
            let lo = win_start.saturating_sub(block_start).min(plain_len);
            let hi = win_end.saturating_sub(block_start).min(plain_len);
            if hi > lo {
                out.write(&plain[usize::try_from(lo).expect("<= bs")..usize::try_from(hi).expect("<= bs")]);
            }
        }
        Ok(())
    }

    impl Guest for Xf {
        fn describe(op: String) -> Descriptor {
            match op.as_str() {
                // Two-pass compression: the index precedes the blocks and
                // sinks can't seek, so input 0 must be re-readable.
                "compress" => Descriptor {
                    seek: SeekClass::Opaque,
                    random_access_inputs: vec![0],
                },
                // The block index is the manifest.
                "decompress" => Descriptor {
                    seek: SeekClass::ManifestSeekable,
                    random_access_inputs: Vec::new(),
                },
                // Unknown ops still need an answer (describe is total);
                // run/serve-range reject them properly.
                _ => Descriptor {
                    seek: SeekClass::Opaque,
                    random_access_inputs: Vec::new(),
                },
            }
        }

        fn run(
            op: String,
            params: Vec<u8>,
            inputs: Vec<Input>,
            outputs: Vec<Sink>,
        ) -> Result<(), String> {
            let [input] = &inputs[..] else {
                return Err(format!("expected 1 input, got {}", inputs.len()));
            };
            let [output] = &outputs[..] else {
                return Err(format!("expected 1 output, got {}", outputs.len()));
            };
            match op.as_str() {
                "compress" => {
                    let params = decode_params(&params)?;
                    compress(&params, expect_random(input)?, output)
                }
                "decompress" => {
                    if !params.is_empty() {
                        return Err("decompress takes no params".into());
                    }
                    decompress(expect_sequential(input)?, output)
                }
                other => Err(format!("unknown op {other:?}")),
            }
        }

        fn serve_range(
            op: String,
            params: Vec<u8>,
            inputs: Vec<Input>,
            output_ix: u32,
            offset: u64,
            len: u64,
            out: Sink,
        ) -> Result<(), String> {
            if op != "decompress" {
                return Err(format!("op {op:?} does not serve ranges"));
            }
            if !params.is_empty() {
                return Err("decompress takes no params".into());
            }
            if output_ix != 0 {
                return Err(format!("no output {output_ix}: decompress is 1-output"));
            }
            let [input] = &inputs[..] else {
                return Err(format!("expected 1 input, got {}", inputs.len()));
            };
            serve_decompressed_range(expect_random(input)?, offset, len, &out)
        }
    }

    export!(Xf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_default_and_roundtrip() {
        assert_eq!(decode_params(&[]), Ok(CompressParams::default()));
        // {1: 4096, 2: 6, 3: 2} in canonical CBOR:
        // a3 01 19 1000 02 06 03 02
        let bytes = [0xa3, 0x01, 0x19, 0x10, 0x00, 0x02, 0x06, 0x03, 0x02];
        assert_eq!(
            decode_params(&bytes),
            Ok(CompressParams {
                block_size: 4096,
                level: 6,
                align: 2
            })
        );
    }

    #[test]
    fn params_reject_noncanonical_and_junk() {
        // {1: 24} encoded non-canonically (0x18 0x18 would be canonical
        // for 24; 0x19 0x00 0x18 is not).
        assert!(decode_params(&[0xa1, 0x01, 0x19, 0x00, 0x18]).is_err());
        // unknown key
        assert!(decode_params(&[0xa1, 0x04, 0x01]).is_err());
        // non-power-of-two block size {1: 1000}: a1 01 19 03e8
        assert!(decode_params(&[0xa1, 0x01, 0x19, 0x03, 0xe8]).is_err());
        // trailing garbage
        assert!(decode_params(&[0xa0, 0x00]).is_err());
        // wrong key order {2:.., 1:..}
        assert!(decode_params(&[0xa2, 0x02, 0x06, 0x01, 0x18, 0x20]).is_err());
    }

    #[test]
    fn header_roundtrip() {
        let h = Header {
            total: 700 * 1024 * 1024,
            block_size: 2048,
            align: 0,
        };
        assert_eq!(parse_header(&encode_header(h)), Ok(h));
        assert!(parse_header(b"NOPE").is_err());
    }

    #[test]
    fn touched_block_math() {
        assert_eq!(touched_blocks(10_000, 2048, 0, 1), Some((0, 0)));
        assert_eq!(touched_blocks(10_000, 2048, 2047, 2), Some((0, 1)));
        assert_eq!(touched_blocks(10_000, 2048, 4096, 2048), Some((2, 2)));
        assert_eq!(touched_blocks(10_000, 2048, 9_999, 100), Some((4, 4)));
        assert_eq!(touched_blocks(10_000, 2048, 10_000, 5), None);
        assert_eq!(touched_blocks(10_000, 2048, 3, 0), None);
    }
}
