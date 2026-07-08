//! Gate for the `xf-cso` component: the contract-surface exerciser.
//! Beyond the usual pinned fixture + determinism, this component is the
//! first to use CBOR params, per-op seek classes, and a
//! `random-access-inputs` declaration (compress reads its input twice) —
//! the gate proves each of those actually works end to end.

use std::io::{Cursor, Write};
use std::sync::{Arc, LazyLock, Mutex};

use datboi_runtime::stream::{
    RangeRead, RangeRequest, SequentialInput, StreamHost, StreamInput, StreamTransform,
};
use datboi_runtime::{Limits, RuntimeError, SeekClass};

const COMPONENT: &[u8] = include_bytes!("../../../transforms/dist/xf_cso.wasm");

/// blake3 of the fixture — the identity a recipe would pin.
const COMPONENT_BLAKE3: &str = "29f77eb2aa56a06519ae7f7dfe7b6e7de280f17cdf5a790a044246b4c025f8c4";

/// A fake disc image: repetitive spans (compress well), a high-entropy
/// span (forces stored-raw blocks), and a non-block-aligned tail.
fn iso() -> Vec<u8> {
    let mut out = Vec::new();
    for sector in 0u32..40 {
        let byte = (sector % 7) as u8;
        out.extend(std::iter::repeat_n(byte, 2048));
    }
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    out.extend((0..16 * 2048).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 24) as u8
    }));
    out.extend(std::iter::repeat_n(0xEDu8, 1234)); // short tail block
    out
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

fn compress(params: &[u8], iso: Vec<u8>) -> Vec<u8> {
    let (host, transform) = shared();
    let out = Collector::default();
    host.run(
        transform,
        "compress",
        params,
        vec![StreamInput::RandomAccess(Box::new(iso))],
        vec![Box::new(out.clone())],
    )
    .expect("compress");
    out.take()
}

fn decompress(cso: Vec<u8>) -> Vec<u8> {
    let (host, transform) = shared();
    let out = Collector::default();
    let len = cso.len() as u64;
    host.run(
        transform,
        "decompress",
        &[],
        vec![StreamInput::Sequential(SequentialInput {
            reader: Box::new(Cursor::new(cso)),
            len,
        })],
        vec![Box::new(out.clone())],
    )
    .expect("decompress");
    out.take()
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
fn describe_is_per_op() {
    let (host, transform) = shared();
    let c = host.describe(transform, "compress").expect("describe");
    assert_eq!(c.seek, SeekClass::Opaque);
    assert_eq!(
        c.random_access_inputs,
        vec![0],
        "compress declares its two-pass input"
    );
    let d = host.describe(transform, "decompress").expect("describe");
    assert_eq!(d.seek, SeekClass::ManifestSeekable);
    assert!(d.random_access_inputs.is_empty());
}

#[test]
fn roundtrip_is_bit_exact_and_deterministic() {
    let image = iso();
    let cso_a = compress(&[], image.clone());
    let cso_b = compress(&[], image.clone());
    assert_eq!(cso_a, cso_b, "deterministic compression");
    assert!(
        cso_a.len() < image.len() / 2,
        "repetitive spans compressed: {} -> {}",
        image.len(),
        cso_a.len()
    );
    assert_eq!(decompress(cso_a), image, "bit-exact roundtrip");
}

#[test]
fn params_change_the_artifact_and_junk_params_error() {
    let image = iso();
    let default_cso = compress(&[], image.clone());
    // {1: 4096} canonical CBOR: block-size 4096.
    let big_blocks = compress(&[0xa1, 0x01, 0x19, 0x10, 0x00], image.clone());
    assert_ne!(default_cso, big_blocks, "params are recipe content");
    assert_eq!(decompress(big_blocks), image, "alt geometry still roundtrips");

    let (host, transform) = shared();
    // Non-canonical encoding of the same params must be REJECTED, not
    // tolerated: params bytes are part of the recipe identity.
    let err = host
        .run(
            transform,
            "compress",
            &[0xa1, 0x01, 0x1a, 0x00, 0x00, 0x10, 0x00],
            vec![StreamInput::RandomAccess(Box::new(iso()))],
            vec![Box::new(Collector::default())],
        )
        .expect_err("non-canonical params");
    assert!(
        matches!(err, RuntimeError::Transform(ref m) if m.contains("non-canonical")),
        "{err:?}"
    );
}

/// D49 seek-equivalence: every serve-range window equals the same slice
/// of the fully-materialized output, including block-straddling and
/// EOF-clamped windows.
#[test]
fn served_ranges_match_materialization() {
    let image = iso();
    let cso = compress(&[], image.clone());
    let (host, transform) = shared();
    let total = image.len() as u64;
    for (offset, len) in [
        (0u64, 1u64),
        (2047, 2),          // block boundary straddle
        (2048 * 39, 4096),  // repetitive->random transition
        (total - 100, 200), // EOF clamp
        (total + 5, 10),    // fully past EOF: empty
        (81_920, 3),        // inside the stored-raw region
    ] {
        let out = Collector::default();
        let inputs: Vec<Box<dyn RangeRead>> = vec![Box::new(cso.clone())];
        host.serve_range(
            transform,
            "decompress",
            &[],
            inputs,
            RangeRequest {
                output_ix: 0,
                offset,
                len,
            },
            Box::new(out.clone()),
        )
        .expect("serve");
        let start = usize::try_from(offset.min(total)).expect("small");
        let end = usize::try_from(offset.saturating_add(len).min(total)).expect("small");
        assert_eq!(out.take(), &image[start..end], "window {offset}+{len}");
    }
}

#[test]
fn compress_refuses_to_serve_ranges() {
    let (host, transform) = shared();
    let inputs: Vec<Box<dyn RangeRead>> = vec![Box::new(iso())];
    let err = host
        .serve_range(
            transform,
            "compress",
            &[],
            inputs,
            RangeRequest {
                output_ix: 0,
                offset: 0,
                len: 16,
            },
            Box::new(Collector::default()),
        )
        .expect_err("opaque op");
    assert!(matches!(err, RuntimeError::Transform(_)), "{err:?}");
}

#[test]
fn corrupt_container_is_a_clean_guest_error() {
    let image = iso();
    let mut cso = compress(&[], image);
    let mid = cso.len() / 2;
    cso[mid] ^= 0xFF;
    let (host, transform) = shared();
    let len = cso.len() as u64;
    let result = host.run(
        transform,
        "decompress",
        &[],
        vec![StreamInput::Sequential(SequentialInput {
            reader: Box::new(Cursor::new(cso)),
            len,
        })],
        vec![Box::new(Collector::default())],
    );
    match result {
        Err(RuntimeError::Transform(_)) => {}
        Ok(()) => {} // flipped byte happened to keep a block valid: D4 catches it
        Err(other) => panic!("never a trap: {other:?}"),
    }
}
