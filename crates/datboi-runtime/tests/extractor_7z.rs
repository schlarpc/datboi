//! Conformance gate for the ex-7z `datboi:extractor@1` component (D110),
//! mirroring the ex-unrar gate (extractor.rs) plus the pieces 7z adds:
//! the SOLID-folder splitter (one decode pass, slot-routed sinks), the
//! D89 subset property over a solid block, empty members, and the
//! polite refusal of coders outside the v1 scope (PPMd fixture).
//!
//! `version.7z` is a solid single-block LZMA archive (headers
//! compressed, the 7z default) holding, in one folder:
//!   VERSION (11 B, "unrar-0.4.0") · a.rom / b.rom (40 000 B, identical
//!   xorshift pattern) · c.txt ("tiny") · sub/d.txt ("nested")
//! plus empty.bin (0 B, no stream) and the directory `sub` (not listed,
//! consumes no ix — D89 identity rule).

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use datboi_runtime::Limits;
use datboi_runtime::extractor::ExtractorHost;
use datboi_runtime::stream::RangeRead;

/// A 'static, Send sink that captures written bytes for inspection.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    fn take(&self) -> Vec<u8> {
        self.0.lock().expect("lock").clone()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const EX_7Z: &[u8] = include_bytes!(concat!(env!("DATBOI_COMPONENTS_DIR"), "/datboi_ex_7z.wasm"));

const VERSION_7Z: &[u8] = include_bytes!("../../datboi-ingest/tests/fixtures/version.7z");
const PPMD_7Z: &[u8] = include_bytes!("../../datboi-ingest/tests/fixtures/ppmd.7z");
const BCJ_7Z: &[u8] = include_bytes!("../../datboi-ingest/tests/fixtures/bcj.7z");
const BCJ2_7Z: &[u8] = include_bytes!("../../datboi-ingest/tests/fixtures/bcj2.7z");
const BCJ2_LZMA_7Z: &[u8] = include_bytes!("../../datboi-ingest/tests/fixtures/bcj2lzma.7z");
const ARM64_7Z: &[u8] = include_bytes!("../../datboi-ingest/tests/fixtures/arm64.7z");
const DELTA_7Z: &[u8] = include_bytes!("../../datboi-ingest/tests/fixtures/delta.7z");
const CHAIN3_7Z: &[u8] = include_bytes!("../../datboi-ingest/tests/fixtures/chain3.7z");

/// The filter fixtures' x86-flavored payload (`prog.bin`): E8 call
/// opcodes with small relative targets in xorshift filler — enough for
/// the branch converters to really rewrite bytes.
fn prog_pattern() -> Vec<u8> {
    let mut state = 0x1234_5678_9ABC_DEF0u64;
    let mut rnd = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut out = Vec::new();
    while out.len() < 120_000 {
        for _ in 0..(rnd() % 12 + 2) {
            out.push((rnd() & 0xFF) as u8);
        }
        out.push(0xE8);
        let rel = (rnd() % 60_000) as i64 - 30_000;
        out.extend_from_slice(&(rel as i32).to_le_bytes());
    }
    out.truncate(120_000);
    out
}

/// The delta fixture's payload (`audio.bin`): integer triangle waves,
/// 4-byte frames.
fn pcm_pattern() -> Vec<u8> {
    let mut out = Vec::with_capacity(120_000);
    for i in 0i64..30_000 {
        let v = ((i * 7) % 4000 - 2000).unsigned_abs() as u16;
        let w = ((i * 13) % 6000 - 3000).unsigned_abs() as u16;
        out.extend_from_slice(&v.to_le_bytes());
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

/// Extract member 0 of a one-member fixture and return its bytes.
fn extract_single(host: &ExtractorHost, fixture: &[u8]) -> Result<Vec<u8>, String> {
    let component = host.load(EX_7Z).expect("load");
    let out = SharedBuf::default();
    host.extract(
        &component,
        vec![Box::new(fixture.to_vec()) as Box<dyn RangeRead>],
        &[],
        vec![(0, Box::new(out.clone()))],
    )
    .map_err(|e| format!("{e}"))?;
    Ok(out.take())
}

/// The fixture's a.rom/b.rom payload (same xorshift the ingest tests use).
fn rom_pattern() -> Vec<u8> {
    let mut state = 0xAAAA_BBBB_CCCC_DDDDu64;
    (0..40_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

fn host() -> ExtractorHost {
    ExtractorHost::new(Limits::default()).expect("host")
}

fn archive() -> Vec<Box<dyn RangeRead>> {
    vec![Box::new(VERSION_7Z.to_vec()) as Box<dyn RangeRead>]
}

/// name → (ix, size, solid) as enumerated.
fn member_map(host: &ExtractorHost) -> HashMap<String, (u32, u64, bool)> {
    let component = host.load(EX_7Z).expect("load");
    host.enumerate(&component, archive(), &[])
        .expect("enumerate")
        .into_iter()
        .map(|m| (m.name.clone(), (m.ix, m.size, m.solid)))
        .collect()
}

#[test]
fn enumerate_lists_files_only_with_stable_ix() {
    let host = host();
    let members = member_map(&host);
    let rom_len = 40_000u64;
    let expect: &[(&str, u64)] = &[
        ("VERSION", 11),
        ("a.rom", rom_len),
        ("b.rom", rom_len),
        ("c.txt", 4),
        ("empty.bin", 0),
        ("sub/d.txt", 6),
    ];
    assert_eq!(members.len(), expect.len(), "directories are not listed");
    for &(name, size) in expect {
        let (_, got, _) = members[name];
        assert_eq!(got, size, "{name}");
    }
    // ix is dense files-only numbering.
    let mut ixs: Vec<u32> = members.values().map(|&(ix, _, _)| ix).collect();
    ixs.sort_unstable();
    assert_eq!(ixs, (0..expect.len() as u32).collect::<Vec<_>>());
    // Solid folder members say so; the streamless empty file does not.
    assert!(members["a.rom"].2, "solid-block member flagged solid");
    assert!(!members["empty.bin"].2, "no-stream member is not solid");
}

#[test]
fn solid_batch_extracts_every_member_byte_exact() {
    let host = host();
    let members = member_map(&host);
    let component = host.load(EX_7Z).expect("load");
    let rom = rom_pattern();
    let want: &[(&str, &[u8])] = &[
        ("VERSION", b"unrar-0.4.0"),
        ("a.rom", &rom),
        ("b.rom", &rom),
        ("c.txt", b"tiny"),
        ("empty.bin", b""),
        ("sub/d.txt", b"nested"),
    ];
    let sinks: Vec<SharedBuf> = want.iter().map(|_| SharedBuf::default()).collect();
    let requests = want
        .iter()
        .zip(&sinks)
        .map(|(&(name, _), sink)| {
            (
                members[name].0,
                Box::new(sink.clone()) as Box<dyn Write + Send>,
            )
        })
        .collect();
    host.extract(&component, archive(), &[], requests)
        .expect("extract");
    for (&(name, bytes), sink) in want.iter().zip(&sinks) {
        assert_eq!(sink.take(), bytes, "{name} bit-exact");
    }
}

#[test]
fn member_bytes_are_a_pure_function_of_ix() {
    // The D89 subset clause, load-bearing for the solid splitter: a
    // one-member request from the middle of the solid block yields the
    // same bytes as the full-batch run.
    let host = host();
    let members = member_map(&host);
    let component = host.load(EX_7Z).expect("load");
    let out = SharedBuf::default();
    host.extract(
        &component,
        archive(),
        &[],
        vec![(members["b.rom"].0, Box::new(out.clone()))],
    )
    .expect("extract");
    assert_eq!(out.take(), rom_pattern(), "subset request, identical bytes");
}

#[test]
fn extraction_is_deterministic() {
    let host = host();
    let members = member_map(&host);
    let component = host.load(EX_7Z).expect("load");
    let run = || {
        let out = SharedBuf::default();
        host.extract(
            &component,
            archive(),
            &[],
            vec![(members["a.rom"].0, Box::new(out.clone()))],
        )
        .expect("extract");
        out.take()
    };
    assert_eq!(run(), run(), "same input → identical output bytes");
}

#[test]
fn empty_batch_is_a_no_op() {
    let host = host();
    let component = host.load(EX_7Z).expect("load");
    host.extract(&component, archive(), &[], Vec::new())
        .expect("empty batch succeeds trivially");
}

#[test]
fn duplicate_request_ix_refuses() {
    let host = host();
    let component = host.load(EX_7Z).expect("load");
    let (a, b) = (SharedBuf::default(), SharedBuf::default());
    let err = host
        .extract(
            &component,
            archive(),
            &[],
            vec![(0, Box::new(a.clone())), (0, Box::new(b))],
        )
        .expect_err("two sinks for one member is ambiguous");
    assert!(a.take().is_empty(), "no bytes on refusal");
    let _ = err;
}

#[test]
fn multi_archive_list_refuses_as_policy() {
    let host = host();
    let component = host.load(EX_7Z).expect("load");
    let archives: Vec<Box<dyn RangeRead>> =
        vec![Box::new(VERSION_7Z.to_vec()), Box::new(VERSION_7Z.to_vec())];
    let err = host
        .enumerate(&component, archives, &[])
        .expect_err("multi-volume is a v1 scope cut");
    assert!(format!("{err}").contains("multi-volume"), "{err}");
}

#[test]
fn nonempty_params_refuse() {
    let host = host();
    let component = host.load(EX_7Z).expect("load");
    let err = host
        .enumerate(&component, archive(), &[0xa0])
        .expect_err("ex-7z takes no params");
    assert!(format!("{err}").contains("params"), "{err}");
}

#[test]
fn out_of_range_member_refuses() {
    let host = host();
    let component = host.load(EX_7Z).expect("load");
    let out = SharedBuf::default();
    let err = host
        .extract(
            &component,
            archive(),
            &[],
            vec![(99, Box::new(out.clone()))],
        )
        .expect_err("out-of-range member must fail");
    assert!(out.take().is_empty(), "no bytes produced on refusal");
    let _ = err;
}

#[test]
fn every_7zdec_coder_shape_round_trips() {
    // Full 7zDec coverage (D108 — no de-scoped fallback): PPMd, the
    // branch filters (x86 with its resumable state, a stateless ISA),
    // Delta, and both BCJ2 trees (Copy substreams and the classic
    // LZMA tree) all decode byte-exact through the STREAMING pipeline.
    let host = host();
    let prog = prog_pattern();
    assert_eq!(
        extract_single(&host, PPMD_7Z).expect("ppmd"),
        b"tiny",
        "PPMd7 main coder"
    );
    assert_eq!(
        extract_single(&host, BCJ_7Z).expect("bcj"),
        prog,
        "x86 BCJ filter over LZMA"
    );
    assert_eq!(
        extract_single(&host, ARM64_7Z).expect("arm64"),
        prog,
        "ARM64 branch filter over LZMA"
    );
    assert_eq!(
        extract_single(&host, BCJ2_LZMA_7Z).expect("bcj2+lzma"),
        prog,
        "BCJ2 tree with LZMA substreams"
    );
    assert_eq!(
        extract_single(&host, DELTA_7Z).expect("delta"),
        pcm_pattern(),
        "Delta filter over LZMA"
    );
}

#[test]
fn unsupported_folder_shape_refuses_politely() {
    // Folder graphs beyond even 7zDec's shapes — a 3-coder chain
    // (Delta+BCJ+LZMA) and 7-Zip's raw-stream BCJ2-only layout (one
    // 4-input coder over four pack streams, which upstream 7zDec
    // refuses too): enumerate parses the header fine, extract refuses
    // with an error (never a trap, never bytes) and the container
    // stays literal.
    let host = host();
    let component = host.load(EX_7Z).expect("load");
    for (name, fixture) in [("chain3", CHAIN3_7Z), ("bcj2-raw", BCJ2_7Z)] {
        let members = host
            .enumerate(&component, vec![Box::new(fixture.to_vec())], &[])
            .expect("header parse is coder-independent");
        assert_eq!(members.len(), 1, "{name}");
        let out = SharedBuf::default();
        let err = host
            .extract(
                &component,
                vec![Box::new(fixture.to_vec())],
                &[],
                vec![(0, Box::new(out.clone()))],
            )
            .expect_err("unsupported folder shape must refuse");
        assert!(out.take().is_empty(), "{name}: no bytes on refusal");
        let _ = err;
    }
}

#[test]
fn corrupt_archive_refuses_whole() {
    let host = host();
    let component = host.load(EX_7Z).expect("load");
    // A valid 7z signature followed by garbage: the header walk must
    // fail (trap or error), never surface bytes.
    let mut bytes = vec![0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
    bytes.extend_from_slice(&[0xFFu8; 64]);
    let result = host.enumerate(&component, vec![Box::new(bytes)], &[]);
    assert!(result.is_err(), "corrupt archive must be refused");
}
