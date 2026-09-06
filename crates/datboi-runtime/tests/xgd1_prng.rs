//! Gate for the `xf-xgd1-prng` component (D111): the XGD1 filler stream
//! as a zero-input `fill` op. Expected bytes come from the SAME crate
//! the component compiles from (the generator the analyzer verifies
//! with natively), so this is a wasm-vs-native equivalence check on top
//! of the pinned fixture + determinism + D49 seek-equivalence.

use std::io::Write;
use std::sync::{Arc, LazyLock, Mutex};

use datboi_runtime::stream::{RangeRead, RangeRequest, StreamHost, StreamTransform};
use datboi_runtime::{Limits, RuntimeError, SeekClass};
use datboi_xf_xgd1_prng::params::Params;
use datboi_xf_xgd1_prng::{Prng, SECTOR_LEN};

/// The nix-built component (D66), embedded at compile time via
/// `DATBOI_COMPONENTS_DIR` — never a checked-in artifact.
const COMPONENT: &[u8] = include_bytes!(concat!(
    env!("DATBOI_COMPONENTS_DIR"),
    "/datboi_xf_xgd1_prng.wasm"
));

/// blake3 of the fixture — the identity a recipe would pin.
const COMPONENT_BLAKE3: &str = "9a1bea864d4b2afca2dc1856450b3b371345ac6b10deeeecc84f83ecdd9e5296";

/// Halo: Combat Evolved (USA) (v1.02), game-partition sector 0 — the
/// seed the prototype recovered and confirmed against sectors 0..31.
const REAL_SEED: u32 = 0x4E99_8EB0;
const REAL_GP0: &[u8; SECTOR_LEN] = include_bytes!("../../datboi-xf-xgd1-prng/data/v102_gp0.bin");

fn native_stream(seed: u32, sectors: u64) -> Vec<u8> {
    let mut prng = Prng::new(seed, 0);
    let mut out = Vec::with_capacity(usize::try_from(sectors).expect("small") * SECTOR_LEN);
    let mut sector = [0u8; SECTOR_LEN];
    for _ in 0..sectors {
        prng.fill_sector(&mut sector);
        out.extend_from_slice(&sector);
    }
    out
}

fn params(seed: u32, sectors: u64) -> Vec<u8> {
    let (buf, n) = Params { seed, sectors }.encode();
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
    assert!(host.describe(transform, "recover").is_err(), "no such op");
}

/// wasm fill == native generator, and the first sector is the real
/// disc's — twice (determinism).
#[test]
fn fill_matches_native_and_the_real_disc() {
    let (host, transform) = shared();
    // 70 sectors crosses the 64-sector write batch inside the guest.
    let expected = native_stream(REAL_SEED, 70);
    assert_eq!(&expected[..SECTOR_LEN], REAL_GP0, "native self-check");
    for _ in 0..2 {
        let out = Collector::default();
        host.run(
            transform,
            "fill",
            &params(REAL_SEED, 70),
            Vec::new(),
            vec![Box::new(out.clone())],
        )
        .expect("fill succeeds");
        assert_eq!(out.take(), expected, "wasm stream equals native stream");
    }
}

/// D49 seek-equivalence: windows inside a sector, straddling sector
/// boundaries, straddling the write batch, EOF clamp, past EOF, whole.
#[test]
fn served_ranges_match_materialization() {
    let (host, transform) = shared();
    let sectors = 70u64;
    let image = native_stream(REAL_SEED, sectors);
    let total = image.len() as u64;
    let sector = SECTOR_LEN as u64;
    for (offset, len) in [
        (0u64, 1u64),
        (100, 200),
        (sector - 1, 2),
        (3 * sector + 7, 5 * sector),
        (63 * sector + 1000, 3000), // across the guest's write batch
        (total - 10, 100),          // EOF clamp
        (total + 7, 4),             // fully past EOF: empty
        (0, total),                 // everything through serve-range
    ] {
        let out = Collector::default();
        let inputs: Vec<Box<dyn RangeRead>> = Vec::new();
        host.serve_range(
            transform,
            "fill",
            &params(REAL_SEED, sectors),
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
fn malformed_params_and_inputs_are_guest_errors() {
    let (host, transform) = shared();
    let err = host
        .run(
            transform,
            "fill",
            &[0xa2, 0x01, 0x18, 0x00, 0x02, 0x01], // non-minimal seed head
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
            &params(REAL_SEED, 0),
            Vec::new(),
            vec![Box::new(Collector::default())],
        )
        .expect_err("zero sectors refuse");
    assert!(matches!(err, RuntimeError::Transform(_)), "{err:?}");
    let err = host
        .run(
            transform,
            "fill",
            &params(REAL_SEED, 1),
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
