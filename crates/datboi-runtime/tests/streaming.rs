//! The M2 gate for the `datboi:transform@2` streaming world (D46/D49):
//! determinism, the exact-read contract, bounded memory, and
//! seek-equivalence. Same fixture discipline as the @1 gate — committed
//! component bytes pinned by blake3, golden output anchors for
//! cross-architecture agreement. The world FROZE 2026-07-07 when this
//! gate plus the full-size exit test (datboi-exec tests/gate.rs) went
//! green; from here, updating the fixture is a format event.

use std::io::{Cursor, Write};
use std::sync::{Arc, LazyLock, Mutex};

use datboi_runtime::stream::{
    RangeRead, RangeRequest, SequentialInput, StreamHost, StreamInput, StreamTransform,
};
use datboi_runtime::{Limits, RuntimeError, SeekClass};

const COMPONENT: &[u8] = include_bytes!("fixtures/xf_reference_stream.wasm");

/// blake3 of the fixture — the identity a recipe would pin.
const COMPONENT_BLAKE3: &str = "da584daadc6d7fe901bb09937e89531736fed4fb466e70b3d7cd39ccd0163716";

/// blake3 of `run("byteswap")` over [`pattern`]`(1 << 20 | 5)` — the
/// cross-architecture anchor.
const BYTESWAP_OUTPUT_BLAKE3: &str =
    "f78cc6339740cd6d7d765bc55de00670d47b01e5c1006ec1393aec3897cbce04";

/// Deterministic pseudo-random bytes covering every value.
fn pattern(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x243F_6A88_85A3_08D3;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// A shared Vec sink: the host consumes it as `Box<dyn Write>`, the test
/// keeps a handle to read the collected bytes.
#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<u8>>>);

impl Collector {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.0.lock().expect("collector"))
    }
}

impl Write for Collector {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("collector").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One compiled component shared across tests: compilation is the slow
/// step and the gate needs hundreds of executions, not hundreds of
/// compiles.
static SHARED: LazyLock<(StreamHost, StreamTransform)> = LazyLock::new(|| {
    let host = StreamHost::new(Limits::default()).expect("deterministic config accepted");
    let transform = host.load(COMPONENT).expect("fixture compiles");
    (host, transform)
});

fn shared() -> (&'static StreamHost, &'static StreamTransform) {
    let (h, t) = &*SHARED;
    (h, t)
}

fn sequential(data: Vec<u8>) -> StreamInput {
    let len = data.len() as u64;
    StreamInput::Sequential(SequentialInput {
        reader: Box::new(Cursor::new(data)),
        len,
    })
}

/// Run an op over owned inputs, collecting the single output.
fn run_collect(op: &str, inputs: Vec<StreamInput>) -> Vec<u8> {
    let (host, transform) = shared();
    let out = Collector::default();
    host.run(transform, op, &[], inputs, vec![Box::new(out.clone())])
        .expect("run succeeds");
    out.take()
}

fn serve_collect(op: &str, inputs: Vec<Box<dyn RangeRead>>, offset: u64, len: u64) -> Vec<u8> {
    let (host, transform) = shared();
    let out = Collector::default();
    host.serve_range(
        transform,
        op,
        &[],
        inputs,
        RangeRequest {
            output_ix: 0,
            offset,
            len,
        },
        Box::new(out.clone()),
    )
    .expect("serve-range succeeds");
    out.take()
}

/// Native swap semantics (spec-pinned in both guest crates) for expected
/// values.
fn native_swap(op: &str, data: &[u8]) -> Vec<u8> {
    let mut buf = data.to_vec();
    match op {
        "bitswap" => buf.iter_mut().for_each(|b| *b = b.reverse_bits()),
        "byteswap" => buf.chunks_exact_mut(2).for_each(|p| p.swap(0, 1)),
        "wordswap" => buf.chunks_exact_mut(4).for_each(<[u8]>::reverse),
        "wordbyteswap" => buf.chunks_exact_mut(4).for_each(|q| {
            q.swap(0, 2);
            q.swap(1, 3);
        }),
        _ => panic!("unknown op"),
    }
    buf
}

#[test]
fn component_bytes_are_pinned() {
    assert_eq!(
        blake3::hash(COMPONENT).to_hex().as_str(),
        COMPONENT_BLAKE3,
        "fixture changed: re-pin the golden constants"
    );
}

#[test]
fn describe_reports_affine() {
    let (host, transform) = shared();
    let d = host.describe(transform, "byteswap").expect("describe");
    assert_eq!(d.seek, SeekClass::Affine);
    assert!(d.random_access_inputs.is_empty());
}

#[test]
fn streamed_swaps_match_native_and_repeat_identically() {
    // Crosses several 64 KiB guest chunks with a ragged tail.
    let data = pattern((1 << 20) | 5);
    for op in ["bitswap", "byteswap", "wordswap", "wordbyteswap"] {
        let first = run_collect(op, vec![sequential(data.clone())]);
        assert_eq!(first, native_swap(op, &data), "{op} semantics");
        let again = run_collect(op, vec![sequential(data.clone())]);
        assert_eq!(first, again, "{op} repeat run diverged");
    }
}

#[test]
fn golden_output_anchor() {
    let out = run_collect("byteswap", vec![sequential(pattern((1 << 20) | 5))]);
    assert_eq!(
        blake3::hash(&out).to_hex().as_str(),
        BYTESWAP_OUTPUT_BLAKE3,
        "cross-architecture determinism anchor broken"
    );
}

#[test]
fn bounded_memory_streams_big_inputs() {
    // 64 MiB through a guest capped at 8 MiB of linear memory: only
    // possible if the transform genuinely streams (whole-buffer would
    // need ≥128 MiB).
    let host = StreamHost::new(Limits {
        memory: 8 << 20,
        ..Limits::default()
    })
    .expect("host");
    let transform = host.load(COMPONENT).expect("compiles");
    let data = pattern(64 << 20);
    let expected = blake3::hash(&native_swap("byteswap", &data));
    let out = Collector::default();
    host.run(
        &transform,
        "byteswap",
        &[],
        vec![sequential(data)],
        vec![Box::new(out.clone())],
    )
    .expect("streaming run fits in 8 MiB of guest memory");
    assert_eq!(blake3::hash(&out.take()), expected);
}

#[test]
fn concat_joins_inputs_in_order() {
    let (a, b, c) = (pattern(70_000), pattern(1), Vec::new());
    let joined = run_collect(
        "concat",
        vec![
            sequential(a.clone()),
            sequential(b.clone()),
            sequential(c.clone()),
        ],
    );
    let expected: Vec<u8> = [a, b, c].concat();
    assert_eq!(joined, expected);
}

#[test]
fn read_contract_holds_under_awkward_sizes() {
    // The probe op asserts read(n) == n until EOF from inside the guest
    // and echoes the bytes; a contract break returns Err.
    let data = pattern(300_001);
    let echoed = run_collect("read-contract-probe", vec![sequential(data.clone())]);
    assert_eq!(echoed, data, "probe must echo exactly its input");
}

/// The D49 seek-equivalence property: serve-range must byte-match slices
/// of the full materialization — with ranges placed at ±1 of every
/// boundary the transform knows about (group boundaries, input joins,
/// EOF) plus deterministic random ranges.
#[test]
fn seek_equivalence_swaps() {
    let data = pattern(200_003); // ragged tail: partial trailing group
    for op in ["bitswap", "byteswap", "wordswap", "wordbyteswap"] {
        let full = run_collect(op, vec![sequential(data.clone())]);

        let mut ranges: Vec<(u64, u64)> = Vec::new();
        // Around group boundaries near the start, a chunk boundary, and EOF.
        for base in [0u64, 65_536, 131_072, 200_000] {
            for off in base.saturating_sub(5)..base + 5 {
                for len in [0u64, 1, 2, 3, 4, 5, 9] {
                    ranges.push((off, len));
                }
            }
        }
        // Past-EOF and huge-len clamping.
        ranges.push((200_003, 10));
        ranges.push((999_999, 4));
        ranges.push((0, u64::MAX));
        // Deterministic pseudo-random ranges.
        let mut state: u64 = 0x9E37_79B9;
        for _ in 0..64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let off = state % 220_000;
            let len = (state >> 32) % 70_000;
            ranges.push((off, len));
        }

        for (off, len) in ranges {
            let got = serve_collect(op, vec![Box::new(data.clone())], off, len);
            let start = (off as usize).min(full.len());
            let end = off.saturating_add(len).min(full.len() as u64) as usize;
            assert_eq!(
                got,
                &full[start..end],
                "{op} range [{off}, {off}+{len}) diverged from materialization"
            );
        }
    }
}

#[test]
fn seek_equivalence_concat() {
    let parts = [pattern(70_001), Vec::new(), pattern(5), pattern(65_536)];
    let full = run_collect("concat", parts.iter().cloned().map(sequential).collect());

    // Joins live at 70001, 70001, 70006, 135542 — probe ±2 around each,
    // plus spans crossing several joins at once.
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for join in [0u64, 70_001, 70_006, 135_542] {
        for off in join.saturating_sub(2)..join + 3 {
            for len in [0u64, 1, 2, 3, 80_000] {
                ranges.push((off, len));
            }
        }
    }
    ranges.push((0, u64::MAX));

    for (off, len) in ranges {
        let inputs: Vec<Box<dyn RangeRead>> = parts
            .iter()
            .map(|p| Box::new(p.clone()) as Box<dyn RangeRead>)
            .collect();
        let got = serve_collect("concat", inputs, off, len);
        let start = (off as usize).min(full.len());
        let end = off.saturating_add(len).min(full.len() as u64) as usize;
        assert_eq!(
            got,
            &full[start..end],
            "concat range [{off}, {off}+{len}) diverged"
        );
    }
}

#[test]
fn greedy_read_traps_at_max_read() {
    // The hostile op asks for u32::MAX bytes in one read; the host's
    // resource-abuse guard must trap before allocating.
    let (host, transform) = shared();
    let err = host
        .run(
            transform,
            "greedy",
            &[],
            vec![sequential(pattern(1024))],
            vec![Box::new(Collector::default())],
        )
        .expect_err("greedy read must trap");
    assert!(matches!(err, RuntimeError::Trap(_)), "got {err:?}");
    assert!(err.to_string().to_lowercase().contains("trap"), "{err}");
}

#[test]
fn fuel_exhaustion_traps() {
    let host = StreamHost::new(Limits {
        fuel: 10_000,
        ..Limits::default()
    })
    .expect("host");
    let transform = host.load(COMPONENT).expect("compiles");
    let err = host
        .run(
            &transform,
            "byteswap",
            &[],
            vec![sequential(pattern(1 << 20))],
            vec![Box::new(Collector::default())],
        )
        .expect_err("must trap on fuel exhaustion");
    assert!(matches!(err, RuntimeError::Trap(_)), "got {err:?}");
}

#[test]
fn unknown_op_and_bad_arity_are_transform_errors() {
    let (host, transform) = shared();
    let err = host
        .run(
            transform,
            "nonsense",
            &[],
            vec![sequential(pattern(8))],
            vec![Box::new(Collector::default())],
        )
        .expect_err("unknown op");
    assert!(matches!(err, RuntimeError::Transform(_)), "got {err:?}");

    let err = host
        .run(
            transform,
            "byteswap",
            &[],
            vec![sequential(pattern(8)), sequential(pattern(8))],
            vec![Box::new(Collector::default())],
        )
        .expect_err("two inputs to a one-input op");
    assert!(matches!(err, RuntimeError::Transform(_)), "got {err:?}");
}

#[test]
fn ambient_imports_still_refused() {
    // The @2 linker carries OUR types interface and nothing else: a
    // component demanding WASI must still fail to link. Stamped inline
    // (D54 attribution fires before linking, and this test is about the
    // LINKER): the component-name payload is subsection 0 wrapping "t".
    let wasi_wanting = wat::parse_str(
        r#"(component
             (import "wasi:cli/environment@0.2.6" (instance))
             (@custom "component-name" "\00\02\01t")
             (@custom "description" "test")
             (@custom "source" "test")
             (@custom "revision" "src:test"))"#,
    )
    .expect("valid wat");
    let (host, _) = shared();
    let err = (|| host.describe(&host.load(&wasi_wanting)?, "anything"))()
        .expect_err("ambient import must be unlinkable at load or link");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("import") || msg.contains("describe"),
        "unexpected failure shape: {msg}"
    );
}
