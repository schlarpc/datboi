//! Gate for the `xf-gc-junk` component (D115): GameCube / Wii junk as
//! a zero-input `fill` op over a disc's address space. Expected bytes
//! come from the SAME crate the component compiles from (the generator
//! the analyzer verifies with natively), so this is a wasm-vs-native
//! equivalence check on top of the pinned fixture + determinism + D49
//! seek-equivalence — and the native side reproduces Dolphin's vectors.

use std::io::Write;
use std::sync::{Arc, LazyLock, Mutex};

use datboi_runtime::stream::{RangeRead, RangeRequest, StreamHost, StreamTransform};
use datboi_runtime::{Limits, RuntimeError, SeekClass};
use datboi_xf_gc_junk::params::Params;
use datboi_xf_gc_junk::{Lfg, SECTOR_LEN, fill_at};

/// The nix-built component (D66), embedded at compile time via
/// `DATBOI_COMPONENTS_DIR` — never a checked-in artifact.
const COMPONENT: &[u8] = include_bytes!(concat!(
    env!("DATBOI_COMPONENTS_DIR"),
    "/datboi_xf_gc_junk.wasm"
));

/// blake3 of the fixture — the identity a recipe would pin.
const COMPONENT_BLAKE3: &str = "7ce9d1ec5e94e569528bf26a2fab5c8d4653d15b448126f9284d6da872922dcd";

/// Dolphin's vector: GALE disc 0 at 0x600000.
const ID: [u8; 4] = *b"GALE";
const VECTOR_OFFSET: u64 = 0x60_0000;
const VECTOR: [u8; 16] = [
    0xE9, 0x47, 0x67, 0xBD, 0x41, 0x50, 0x4D, 0x5D, 0x61, 0x48, 0xB1, 0x99, 0xA0, 0x12, 0x0C, 0xBA,
];

fn native_stream(len: u64) -> Vec<u8> {
    let mut out = vec![0u8; usize::try_from(len).expect("small")];
    fill_at(&mut Lfg::default(), ID, 0, 0, &mut out);
    out
}

fn params(len: u64) -> Vec<u8> {
    let (buf, n) = Params {
        id: ID,
        disc: 0,
        len,
    }
    .encode();
    buf[..n].to_vec()
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

#[test]
fn component_bytes_are_pinned() {
    assert_eq!(
        blake3::hash(COMPONENT).to_hex().as_str(),
        COMPONENT_BLAKE3,
        "fixture changed: re-pin the golden constants"
    );
}

#[test]
fn describe_reports_affine_with_no_inputs() {
    let (host, transform) = shared();
    let d = host.describe(transform, "fill").expect("describe");
    assert_eq!(d.seek, SeekClass::Affine);
    assert!(d.random_access_inputs.is_empty());
    assert!(host.describe(transform, "check").is_err(), "no such op");
}

/// wasm fill == native generator across several reseeds, the stream at
/// Dolphin's vector offset is Dolphin's bytes — twice (determinism).
#[test]
fn fill_matches_native_and_the_reference_vector() {
    let (host, transform) = shared();
    // Past the vector offset by a few sectors: crosses many reseeds and
    // the guest's 128 KiB write batch.
    let len = VECTOR_OFFSET + 3 * SECTOR_LEN as u64 + 100;
    let expected = native_stream(len);
    let at = usize::try_from(VECTOR_OFFSET).expect("small");
    assert_eq!(&expected[at..at + 16], &VECTOR, "native self-check");
    for _ in 0..2 {
        let out = Collector::default();
        host.run(
            transform,
            "fill",
            &params(len),
            Vec::new(),
            vec![Box::new(out.clone())],
        )
        .expect("fill succeeds");
        assert_eq!(out.take(), expected, "wasm stream equals native stream");
    }
}

/// D49 seek-equivalence: windows inside a sector, straddling reseeds,
/// straddling the write batch, EOF clamp, past EOF, whole.
#[test]
fn served_ranges_match_materialization() {
    let (host, transform) = shared();
    let len = 9 * SECTOR_LEN as u64 + 1234;
    let image = native_stream(len);
    let sector = SECTOR_LEN as u64;
    for (offset, wlen) in [
        (0u64, 1u64),
        (100, 200),
        (sector - 1, 2),
        (3 * sector + 7, 2 * sector),
        (4 * sector - 100, 200 * 1024), // across the guest's write batch
        (len - 10, 100),                // EOF clamp
        (len + 7, 4),                   // fully past EOF: empty
        (0, len),                       // everything through serve-range
    ] {
        let out = Collector::default();
        let inputs: Vec<Box<dyn RangeRead>> = Vec::new();
        host.serve_range(
            transform,
            "fill",
            &params(len),
            inputs,
            RangeRequest {
                output_ix: 0,
                offset,
                len: wlen,
            },
            Box::new(out.clone()),
        )
        .expect("serve");
        let start = usize::try_from(offset.min(len)).expect("small");
        let end = usize::try_from(offset.saturating_add(wlen).min(len)).expect("small");
        assert_eq!(out.take(), &image[start..end], "window {offset}+{wlen}");
    }
}

#[test]
fn malformed_params_and_inputs_are_guest_errors() {
    let (host, transform) = shared();
    let err = host
        .run(
            transform,
            "fill",
            &[0xa3, 0x01, 0x18, 0x00, 0x02, 0x00, 0x03, 0x01], // non-minimal id head
            Vec::new(),
            vec![Box::new(Collector::default())],
        )
        .expect_err("non-canonical params refuse");
    assert!(
        matches!(err, RuntimeError::Transform(ref m) if m.contains("non-minimal")),
        "{err:?}"
    );
    let err = host
        .run(
            transform,
            "fill",
            &params(0),
            Vec::new(),
            vec![Box::new(Collector::default())],
        )
        .expect_err("zero length refuses");
    assert!(matches!(err, RuntimeError::Transform(_)), "{err:?}");
    let err = host
        .run(
            transform,
            "fill",
            &params(1),
            Vec::new(),
            vec![
                Box::new(Collector::default()),
                Box::new(Collector::default()),
            ],
        )
        .expect_err("two outputs refuse");
    assert!(
        matches!(err, RuntimeError::Transform(ref m) if m.contains("1 output")),
        "{err:?}"
    );
}
