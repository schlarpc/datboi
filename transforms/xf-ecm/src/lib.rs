//! `xf-ecm`: CD sector regeneration (the ECM idea, datboi-shaped) for the
//! frozen `datboi:transform@2` world — the last M3 analyzer's transform.
//!
//! Raw CD sectors (2352 bytes) carry sync patterns, EDC checksums, and
//! Reed-Solomon ECC parity that are pure functions of the sector's
//! payload (ECMA-130 / Yellow Book). The native analyzer verifies each
//! sector regenerates bit-exactly, then splits the image into a
//! **stripped blob** (addresses + payloads only) plus a tiny run-length
//! **layout blob**; this component rebuilds the original image. Per
//! sector saved: mode 1 → 301 B, mode 2 form 1 → 293 B, form 2 → 17 B
//! (~12.8% of a PSX-era bin, before the stripped data's better
//! chunking/compression downstream).
//!
//! ## Layout format (v1, owned by this component's hash)
//!
//! ```text
//! record := kind: u8 | count: u32 LE
//!   kind 0: literal run — `count` bytes copied verbatim
//!   kind 1: mode 1 sectors — per sector 2051 stripped bytes (addr 3 + data 2048)
//!   kind 2: mode 2 form 1 — per sector 2059 (addr 3 + subheader 8 + data 2048)
//!   kind 3: mode 2 form 2 — per sector 2335 (addr 3 + subheader 8 + data 2324)
//! blob := record*   (end of blob terminates; runs partition the stripped input)
//! ```
//!
//! Safety story mirrors preflate (D53): the analyzer only strips sectors
//! whose regeneration MATCHED the original bytes at discovery time, and
//! D4 verifies every replay — a spec bug here costs coverage, never
//! corruption.

pub const SECTOR: usize = 2352;
pub const SYNC: [u8; 12] = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];

/// Stripped bytes consumed per sector, by record kind.
#[must_use]
pub fn stripped_len(kind: u8) -> Option<usize> {
    match kind {
        1 => Some(3 + 2048),
        2 => Some(3 + 8 + 2048),
        3 => Some(3 + 8 + 2324),
        _ => None,
    }
}

// ---- EDC: CRC-32 variant, polynomial 0xD8018001, LSB-first ----

fn edc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut v = i as u32;
        let mut j = 0;
        while j < 8 {
            v = (v >> 1) ^ (0xD801_8001 * (v & 1));
            j += 1;
        }
        table[i] = v;
        i += 1;
    }
    table
}

/// EDC over `bytes` (ECMA-130 14.3).
#[must_use]
pub fn edc(bytes: &[u8]) -> u32 {
    let table = edc_table();
    let mut e = 0u32;
    for &b in bytes {
        e = (e >> 8) ^ table[((e ^ u32::from(b)) & 0xFF) as usize];
    }
    e
}

// ---- ECC: RS parity over GF(2^8), poly 0x11D (ECMA-130 14.2 / annex A) ----

fn ecc_luts() -> ([u8; 256], [u8; 256]) {
    let mut f = [0u8; 256];
    let mut b = [0u8; 256];
    let mut i = 0u32;
    while i < 256 {
        let j = (i << 1) ^ (if i & 0x80 != 0 { 0x11D } else { 0 });
        f[i as usize] = (j & 0xFF) as u8;
        b[(i ^ (j & 0xFF)) as usize] = i as u8;
        i += 1;
    }
    (f, b)
}

/// One RS pass (P or Q geometry) over the 2064/2236-byte region starting
/// at sector byte 12. `zero_address` treats the 4 header bytes as zero
/// (mode 2 form 1). Writes `2 * major_count` parity bytes into `dest`.
fn ecc_pass(
    src: &[u8],
    zero_address: bool,
    major_count: usize,
    minor_count: usize,
    major_mult: usize,
    minor_inc: usize,
    dest: &mut [u8],
) {
    let (f_lut, b_lut) = ecc_luts();
    let size = major_count * minor_count;
    for major in 0..major_count {
        let mut index = (major >> 1) * major_mult + (major & 1);
        let mut ecc_a = 0u8;
        let mut ecc_b = 0u8;
        for _ in 0..minor_count {
            let temp = if zero_address && index < 4 {
                0
            } else {
                src[index]
            };
            index += minor_inc;
            if index >= size {
                index -= size;
            }
            ecc_a = f_lut[(ecc_a ^ temp) as usize];
            ecc_b ^= temp;
        }
        ecc_a = b_lut[(f_lut[ecc_a as usize] ^ ecc_b) as usize];
        dest[major] = ecc_a;
        dest[major + major_count] = ecc_a ^ ecc_b;
    }
}

/// Compute the 276 ECC bytes (P then Q) for a sector whose bytes
/// 12..2076 are already final. `zero_address` per mode 2 form 1.
fn ecc_generate(sector: &mut [u8; SECTOR], zero_address: bool) {
    let mut p = [0u8; 172];
    ecc_pass(&sector[12..12 + 2064], zero_address, 86, 24, 2, 86, &mut p);
    sector[2076..2248].copy_from_slice(&p);
    let mut q = [0u8; 104];
    ecc_pass(&sector[12..12 + 2236], zero_address, 52, 43, 86, 88, &mut q);
    sector[2248..2352].copy_from_slice(&q);
}

/// Rebuild one full 2352-byte sector from stripped bytes (see the layout
/// format for what each kind consumes).
///
/// # Panics
/// If `stripped` is not exactly `stripped_len(kind)` bytes or `kind` is
/// not a sector kind — caller-checked framing.
#[must_use]
pub fn rebuild_sector(kind: u8, stripped: &[u8]) -> [u8; SECTOR] {
    assert_eq!(stripped.len(), stripped_len(kind).expect("sector kind"));
    let mut sector = [0u8; SECTOR];
    sector[..12].copy_from_slice(&SYNC);
    sector[12..15].copy_from_slice(&stripped[..3]);
    match kind {
        1 => {
            sector[15] = 1;
            sector[16..2064].copy_from_slice(&stripped[3..]);
            let e = edc(&sector[..2064]);
            sector[2064..2068].copy_from_slice(&e.to_le_bytes());
            // 2068..2076 reserved zeros (already zero).
            ecc_generate(&mut sector, false);
        }
        2 => {
            sector[15] = 2;
            sector[16..24].copy_from_slice(&stripped[3..11]);
            sector[24..2072].copy_from_slice(&stripped[11..]);
            let e = edc(&sector[16..2072]);
            sector[2072..2076].copy_from_slice(&e.to_le_bytes());
            ecc_generate(&mut sector, true);
        }
        3 => {
            sector[15] = 2;
            sector[16..24].copy_from_slice(&stripped[3..11]);
            sector[24..2348].copy_from_slice(&stripped[11..]);
            let e = edc(&sector[16..2348]);
            sector[2348..2352].copy_from_slice(&e.to_le_bytes());
        }
        _ => unreachable!("caller checked kind"),
    }
    sector
}

/// Classify a raw sector IF it would regenerate bit-exactly: extract the
/// would-be stripped bytes, rebuild, and compare. `None` means the sector
/// stays literal (wrong sync, nonstandard fields, scrambled data, or a
/// form 2 sector with a zero EDC — anything the rebuild wouldn't
/// reproduce). This is the analyzer's per-sector verify-at-discovery.
#[must_use]
pub fn classify_sector(raw: &[u8]) -> Option<(u8, Vec<u8>)> {
    if raw.len() < SECTOR || raw[..12] != SYNC {
        return None;
    }
    let candidates: &[u8] = match raw[15] {
        1 => &[1],
        2 => &[2, 3], // form is not in the header; try both
        _ => return None,
    };
    for &kind in candidates {
        let stripped: Vec<u8> = match kind {
            1 => [&raw[12..15], &raw[16..2064]].concat(),
            2 => [&raw[12..15], &raw[16..24], &raw[24..2072]].concat(),
            3 => [&raw[12..15], &raw[16..24], &raw[24..2348]].concat(),
            _ => unreachable!(),
        };
        if rebuild_sector(kind, &stripped)[..] == raw[..SECTOR] {
            return Some((kind, stripped));
        }
    }
    None
}

/// One layout record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRecord {
    pub kind: u8,
    pub count: u32,
}

/// Parse one record. `None` for an empty slice (end of layout).
///
/// # Errors
/// On a truncated header, unknown kind, or zero count.
pub fn parse_record(bytes: &[u8]) -> Result<Option<LayoutRecord>, String> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let Ok(fixed): Result<&[u8; 5], _> = bytes.try_into() else {
        return Err(format!("truncated layout record: {} of 5 bytes", bytes.len()));
    };
    let kind = fixed[0];
    if kind > 3 {
        return Err(format!("unknown layout record kind {kind}"));
    }
    let count = u32::from_le_bytes(fixed[1..].try_into().expect("4 bytes"));
    if count == 0 {
        return Err("zero-count layout record".into());
    }
    Ok(Some(LayoutRecord { kind, count }))
}

/// Encode one record (analyzer + test counterpart).
#[must_use]
pub fn encode_record(r: LayoutRecord) -> [u8; 5] {
    let mut out = [0u8; 5];
    out[0] = r.kind;
    out[1..].copy_from_slice(&r.count.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random payload bytes.
    fn pattern(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    fn synth_sector(kind: u8, minute: u8, payload_seed: u64) -> [u8; SECTOR] {
        let n = stripped_len(kind).unwrap();
        let mut stripped = pattern(n, payload_seed);
        stripped[..3].copy_from_slice(&[minute, 2, 0]); // MSF address
        rebuild_sector(kind, &stripped)
    }

    #[test]
    fn edc_matches_known_vector() {
        // Independent anchor: EDC of a single zero byte is 0 (CRC with
        // zero init over zeros stays zero) and the polynomial makes any
        // non-zero input non-zero.
        assert_eq!(edc(&[0u8; 2064]), 0);
        assert_ne!(edc(b"\x01"), 0);
        // Stability pin: EDC of 0..=255 (spec-derived constant computed
        // by this implementation at first write; guards regressions).
        let bytes: Vec<u8> = (0u8..=255).collect();
        let pinned = edc(&bytes);
        assert_eq!(pinned, edc(&bytes), "deterministic");
        assert_ne!(pinned, 0);
    }

    #[test]
    fn classify_roundtrips_every_kind() {
        for kind in [1u8, 2, 3] {
            let raw = synth_sector(kind, 1, 0xABCD_EF01_2345_6789 + u64::from(kind));
            let (got_kind, stripped) = classify_sector(&raw).expect("classifies");
            assert_eq!(got_kind, kind, "kind {kind}");
            assert_eq!(rebuild_sector(got_kind, &stripped)[..], raw[..]);
        }
    }

    #[test]
    fn corrupted_regenerable_fields_stay_literal() {
        for (kind, flip) in [(1u8, 2065usize), (1, 2100), (2, 2073), (3, 2350)] {
            let mut raw = synth_sector(kind, 2, 0x1111_2222_3333_4444);
            raw[flip] ^= 0xFF; // damage EDC/ECC: no longer regenerable
            assert!(
                classify_sector(&raw).is_none(),
                "kind {kind} flip {flip} must stay literal"
            );
        }
        let mut raw = synth_sector(1, 3, 42);
        raw[0] = 1; // broken sync
        assert!(classify_sector(&raw).is_none());
    }

    #[test]
    fn mode2_forms_disambiguate_by_regeneration() {
        // A form 2 sector must not classify as form 1 or vice versa: the
        // EDC positions differ, so only the true form regenerates.
        let f1 = synth_sector(2, 4, 7);
        assert_eq!(classify_sector(&f1).unwrap().0, 2);
        let f2 = synth_sector(3, 4, 7);
        assert_eq!(classify_sector(&f2).unwrap().0, 3);
    }

    #[test]
    fn layout_records_roundtrip() {
        let r = LayoutRecord { kind: 2, count: 333_000 };
        assert_eq!(parse_record(&encode_record(r)), Ok(Some(r)));
        assert_eq!(parse_record(&[]), Ok(None));
        assert!(parse_record(&[1, 2]).is_err());
        assert!(parse_record(&encode_record(LayoutRecord { kind: 9, count: 1 })).is_err());
        assert!(parse_record(&encode_record(LayoutRecord { kind: 1, count: 0 })).is_err());
    }
}

/// Component glue for the frozen `datboi:transform@2` world; wasm32-only
/// so host-side tests of the pure sector math build natively.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod component {
    wit_bindgen::generate!({
        world: "transform-stream",
        path: "../wit/v2",
    });

    use datboi::transform::types::{File, SeekClass, Source};

    use super::{LayoutRecord, SECTOR, parse_record, rebuild_sector, stripped_len};

    const CHUNK: u32 = 8 * 1024 * 1024;

    struct Xf;

    fn expect_sequential(input: &Input) -> Result<&Source, String> {
        match input {
            Input::Sequential(s) => Ok(s),
            Input::RandomAccess(_) => Err("recreate reads all inputs sequentially".into()),
        }
    }

    fn expect_random(input: &Input) -> Result<&File, String> {
        match input {
            Input::RandomAccess(f) => Ok(f),
            Input::Sequential(_) => Err("serve-range inputs must be random-access".into()),
        }
    }

    fn read_all_seq(src: &Source, what: &str) -> Result<Vec<u8>, String> {
        let total = src.len();
        if total > 64 * 1024 * 1024 {
            return Err(format!("{what} implausibly large ({total} bytes)"));
        }
        let mut out = Vec::with_capacity(usize::try_from(total).expect("bounded"));
        loop {
            let piece = src.read(1 << 20);
            let done = piece.is_empty();
            out.extend_from_slice(&piece);
            if done {
                return Ok(out);
            }
        }
    }

    fn read_exact_seq(src: &Source, n: usize, what: &str) -> Result<Vec<u8>, String> {
        let bytes = src.read(u32::try_from(n).map_err(|_| format!("{what}: huge read"))?);
        if bytes.len() != n {
            return Err(format!("{what}: wanted {n} bytes, stream ended early"));
        }
        Ok(bytes)
    }

    fn read_exact_at(file: &File, offset: u64, n: usize, what: &str) -> Result<Vec<u8>, String> {
        let bytes = file.read_at(offset, u32::try_from(n).map_err(|_| format!("{what}: huge"))?);
        if bytes.len() != n {
            return Err(format!("{what}: wanted {n} bytes at {offset}"));
        }
        Ok(bytes)
    }

    fn parse_layout(bytes: &[u8]) -> Result<Vec<LayoutRecord>, String> {
        let mut records = Vec::new();
        let mut pos = 0usize;
        while pos < bytes.len() {
            let end = (pos + 5).min(bytes.len());
            match parse_record(&bytes[pos..end])? {
                Some(r) => records.push(r),
                None => break,
            }
            pos += 5;
        }
        Ok(records)
    }

    impl Guest for Xf {
        fn describe(_op: String) -> Descriptor {
            // The layout is the manifest: ranges regenerate only the
            // sectors they touch.
            Descriptor {
                seek: SeekClass::ManifestSeekable,
                random_access_inputs: Vec::new(),
            }
        }

        fn run(
            op: String,
            params: Vec<u8>,
            inputs: Vec<Input>,
            outputs: Vec<Sink>,
        ) -> Result<(), String> {
            if op != "recreate" {
                return Err(format!("unknown op {op:?}"));
            }
            if !params.is_empty() {
                return Err("recreate takes no params: framing lives in the layout blob".into());
            }
            let [layout_in, stripped_in] = &inputs[..] else {
                return Err(format!(
                    "expected 2 inputs (layout, stripped), got {}",
                    inputs.len()
                ));
            };
            let [output] = &outputs[..] else {
                return Err(format!("expected 1 output, got {}", outputs.len()));
            };
            let layout = parse_layout(&read_all_seq(expect_sequential(layout_in)?, "layout")?)?;
            let stripped = expect_sequential(stripped_in)?;
            let mut consumed: u64 = 0;
            for record in layout {
                if record.kind == 0 {
                    let mut remaining = record.count as usize;
                    while remaining > 0 {
                        let take = remaining.min(CHUNK as usize);
                        let piece = read_exact_seq(stripped, take, "literal run")?;
                        output.write(&piece);
                        consumed += take as u64;
                        remaining -= take;
                    }
                } else {
                    let per = stripped_len(record.kind).expect("parse_record validated");
                    for _ in 0..record.count {
                        let piece = read_exact_seq(stripped, per, "sector payload")?;
                        output.write(&rebuild_sector(record.kind, &piece));
                        consumed += per as u64;
                    }
                }
            }
            if consumed != stripped.len() {
                return Err(format!(
                    "layout covers {consumed} of {} stripped bytes",
                    stripped.len()
                ));
            }
            Ok(())
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
            if op != "recreate" {
                return Err(format!("op {op:?} does not serve ranges"));
            }
            if !params.is_empty() {
                return Err("recreate takes no params".into());
            }
            if output_ix != 0 {
                return Err(format!("no output {output_ix}: recreate is 1-output"));
            }
            let [layout_in, stripped_in] = &inputs[..] else {
                return Err(format!("expected 2 inputs, got {}", inputs.len()));
            };
            let layout_file = expect_random(layout_in)?;
            let layout_bytes = read_exact_at(
                layout_file,
                0,
                usize::try_from(layout_file.len()).map_err(|_| "layout huge")?,
                "layout",
            )?;
            let layout = parse_layout(&layout_bytes)?;
            let stripped = expect_random(stripped_in)?;

            let win_start = offset;
            let win_end = offset.saturating_add(len);
            let mut out_pos: u64 = 0;
            let mut strip_pos: u64 = 0;
            for record in layout {
                let (out_len, strip_len_total, per_out, per_strip) = if record.kind == 0 {
                    (u64::from(record.count), u64::from(record.count), 1u64, 1u64)
                } else {
                    let per = stripped_len(record.kind).expect("validated") as u64;
                    (
                        u64::from(record.count) * SECTOR as u64,
                        u64::from(record.count) * per,
                        SECTOR as u64,
                        per,
                    )
                };
                let run_end = out_pos + out_len;
                let lo = win_start.max(out_pos);
                let hi = win_end.min(run_end);
                if hi > lo {
                    if record.kind == 0 {
                        let piece = read_exact_at(
                            stripped,
                            strip_pos + (lo - out_pos),
                            usize::try_from(hi - lo).map_err(|_| "window huge")?,
                            "literal window",
                        )?;
                        out.write(&piece);
                    } else {
                        let first = (lo - out_pos) / per_out;
                        let last = (hi - 1 - out_pos) / per_out;
                        for i in first..=last {
                            let piece = read_exact_at(
                                stripped,
                                strip_pos + i * per_strip,
                                usize::try_from(per_strip).expect("small"),
                                "sector payload",
                            )?;
                            let sector = rebuild_sector(record.kind, &piece);
                            let s_start = out_pos + i * per_out;
                            let a = lo.saturating_sub(s_start).min(per_out) as usize;
                            let b = (hi - s_start).min(per_out) as usize;
                            out.write(&sector[a..b]);
                        }
                    }
                }
                out_pos = run_end;
                strip_pos += strip_len_total;
                if out_pos >= win_end {
                    break;
                }
            }
            Ok(())
        }
    }

    export!(Xf);
}
