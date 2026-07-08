//! Gate for the `xf-preflate` component (D53): the wild-zip rebuild path.
//! Same fixture discipline as the reference gates — committed component
//! bytes pinned by blake3 — but the golden anchor here is *self-checking*:
//! `recreate`'s output must equal the committed deflate stream byte for
//! byte, which is the whole point of the transform.
//!
//! Vectors: `preflate_member.deflate` is miniz level-6 over the shared
//! xorshift `pattern(1 << 20 | 5)`; `preflate_corrections.bin` was framed
//! by the native split with deliberately small windows (11 frames), so the
//! gate exercises multi-frame walking and deflate bit state carried across
//! frame boundaries.

use std::io::{Cursor, Write};
use std::sync::{Arc, LazyLock, Mutex};

use datboi_runtime::stream::{
    RangeRead, RangeRequest, SequentialInput, StreamHost, StreamInput, StreamTransform,
};
use datboi_runtime::{Limits, RuntimeError, SeekClass};

const COMPONENT: &[u8] = include_bytes!("../../../transforms/dist/xf_preflate.wasm");

/// blake3 of the fixture — the identity a recipe would pin.
const COMPONENT_BLAKE3: &str = "2c35595b095eeabbe3d35db0a1f486971b6796592384be8b36a0ab03aa0e418d";

const MEMBER: &[u8] = include_bytes!("fixtures/preflate_member.deflate");
const CORRECTIONS: &[u8] = include_bytes!("fixtures/preflate_corrections.bin");

/// Deterministic pseudo-random bytes — same generator as the reference
/// gates; the member fixture compresses exactly this.
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

fn recreate_inputs() -> Vec<StreamInput> {
    vec![
        sequential(CORRECTIONS.to_vec()),
        sequential(pattern(1 << 20 | 5)),
    ]
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
fn describe_reports_opaque() {
    let (host, transform) = shared();
    let d = host.describe(transform, "recreate").expect("describe");
    assert_eq!(d.seek, SeekClass::Opaque);
    assert!(d.random_access_inputs.is_empty());
}

/// The self-checking golden anchor: corrections + plaintext recreate the
/// committed deflate stream bit-for-bit, twice (determinism).
#[test]
fn recreate_is_bit_exact_and_deterministic() {
    let (host, transform) = shared();
    for _ in 0..2 {
        let out = Collector::default();
        host.run(
            transform,
            "recreate",
            &[],
            recreate_inputs(),
            vec![Box::new(out.clone())],
        )
        .expect("recreate succeeds");
        assert_eq!(out.take(), MEMBER, "bit-exact recreation");
    }
}

#[test]
fn truncated_plaintext_is_a_clean_guest_error() {
    let (host, transform) = shared();
    let mut plain = pattern(1 << 20 | 5);
    plain.truncate(plain.len() - 7);
    let out = Collector::default();
    let err = host
        .run(
            transform,
            "recreate",
            &[],
            vec![sequential(CORRECTIONS.to_vec()), sequential(plain)],
            vec![Box::new(out.clone())],
        )
        .expect_err("must fail");
    assert!(
        matches!(err, RuntimeError::Transform(ref msg) if msg.contains("ended early")),
        "clean guest-reported error, not a trap: {err:?}"
    );
}

#[test]
fn corrupt_corrections_never_panic() {
    let (host, transform) = shared();
    // Flip a byte inside the first frame's corrections payload; the guest
    // must fail with an error (or produce wrong bytes the D4 hash check
    // would catch) — never trap the runtime.
    let mut corrections = CORRECTIONS.to_vec();
    corrections[10] ^= 0xFF;
    let out = Collector::default();
    let result = host.run(
        transform,
        "recreate",
        &[],
        vec![
            sequential(corrections),
            sequential(pattern(1 << 20 | 5)),
        ],
        vec![Box::new(out.clone())],
    );
    match result {
        Err(RuntimeError::Transform(_)) => {}
        Ok(()) => assert_ne!(out.take(), MEMBER, "corruption cannot yield the true bytes"),
        Err(other) => panic!("expected a guest error or wrong bytes, got {other:?}"),
    }
}

#[test]
fn truncated_corrections_header_is_malformed() {
    let (host, transform) = shared();
    let corrections = CORRECTIONS[..5].to_vec(); // mid-header
    let err = host
        .run(
            transform,
            "recreate",
            &[],
            vec![sequential(corrections), sequential(pattern(64))],
            vec![Box::new(Collector::default())],
        )
        .expect_err("must fail");
    assert!(
        matches!(err, RuntimeError::Transform(ref msg) if msg.contains("truncated")),
        "{err:?}"
    );
}

#[test]
fn serve_range_refuses_opaque() {
    let (host, transform) = shared();
    let inputs: Vec<Box<dyn RangeRead>> =
        vec![Box::new(CORRECTIONS.to_vec()), Box::new(pattern(1024))];
    let err = host
        .serve_range(
            transform,
            "recreate",
            &[],
            inputs,
            RangeRequest {
                output_ix: 0,
                offset: 0,
                len: 16,
            },
            Box::new(Collector::default()),
        )
        .expect_err("opaque must refuse");
    assert!(matches!(err, RuntimeError::Transform(_)), "{err:?}");
}

#[test]
fn unknown_op_and_params_are_guest_errors() {
    let (host, transform) = shared();
    let err = host
        .run(
            transform,
            "inflate",
            &[],
            recreate_inputs(),
            vec![Box::new(Collector::default())],
        )
        .expect_err("unknown op");
    assert!(matches!(err, RuntimeError::Transform(ref m) if m.contains("unknown op")));

    let err = host
        .run(
            transform,
            "recreate",
            &[1, 2, 3],
            recreate_inputs(),
            vec![Box::new(Collector::default())],
        )
        .expect_err("params must be empty");
    assert!(matches!(err, RuntimeError::Transform(ref m) if m.contains("no params")));
}
