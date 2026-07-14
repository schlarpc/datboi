//! Gate for the `xf-ecm` component: CD sector regeneration (M3's last
//! analyzer). The vectors are built NATIVELY with the same crate the
//! component compiles from, so this gate is a wasm-vs-native
//! equivalence check on top of the usual pinned fixture + determinism +
//! D49 seek-equivalence.

use std::io::{Cursor, Write};
use std::sync::{Arc, LazyLock, Mutex};

use datboi_runtime::stream::{
    RangeRead, RangeRequest, SequentialInput, StreamHost, StreamInput, StreamTransform,
};
use datboi_runtime::{Limits, RuntimeError, SeekClass};
use datboi_xf_ecm::{
    LayoutRecord, SECTOR, classify_sector, encode_record, rebuild_sector, stripped_len,
};

/// The nix-built `xf-ecm` component (D66), embedded at compile time
/// via `DATBOI_COMPONENTS_DIR` (build.rs re-exports it) — never a
/// checked-in artifact.
const COMPONENT: &[u8] = include_bytes!(concat!(
    env!("DATBOI_COMPONENTS_DIR"),
    "/datboi_xf_ecm.wasm"
));

/// blake3 of the fixture — the identity a recipe would pin.
const COMPONENT_BLAKE3: &str = "cfd94cf5f08d6846172a7aade1d415d6debb343acb316cc9e5d3b5e884c9de20";

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

/// Build a mixed image natively: mode 1 run, literal junk, mode 2
/// form 1 and form 2 runs, and a trailing sub-sector literal.
/// Returns (image, layout blob, stripped blob).
fn vectors() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut image = Vec::new();
    let mut layout = Vec::new();
    let mut stripped = Vec::new();
    fn push_sectors(
        kind: u8,
        count: u32,
        seed: u64,
        image: &mut Vec<u8>,
        layout: &mut Vec<u8>,
        stripped: &mut Vec<u8>,
    ) {
        for i in 0..count {
            let n = stripped_len(kind).expect("kind");
            let mut s = pattern(n, seed + u64::from(i));
            s[..3].copy_from_slice(&[0, 2 + (i / 75) as u8, (i % 75) as u8]);
            let sector = rebuild_sector(kind, &s);
            assert_eq!(
                classify_sector(&sector).expect("classifies").0,
                kind,
                "native self-check"
            );
            image.extend_from_slice(&sector);
            stripped.extend_from_slice(&s);
        }
        layout.extend_from_slice(&encode_record(LayoutRecord { kind, count }));
    }
    push_sectors(1, 5, 0xAAAA_0001, &mut image, &mut layout, &mut stripped);
    // A literal run that is NOT sector-shaped (e.g. a scrambled region).
    let junk = pattern(3000, 0xBBBB_0002);
    layout.extend_from_slice(&encode_record(LayoutRecord {
        kind: 0,
        count: junk.len() as u32,
    }));
    image.extend_from_slice(&junk);
    stripped.extend_from_slice(&junk);
    push_sectors(2, 4, 0xCCCC_0003, &mut image, &mut layout, &mut stripped);
    push_sectors(3, 3, 0xDDDD_0004, &mut image, &mut layout, &mut stripped);
    let tail = pattern(1234, 0xEEEE_0005);
    layout.extend_from_slice(&encode_record(LayoutRecord {
        kind: 0,
        count: tail.len() as u32,
    }));
    image.extend_from_slice(&tail);
    stripped.extend_from_slice(&tail);
    (image, layout, stripped)
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

#[test]
fn component_bytes_are_pinned() {
    assert_eq!(
        blake3::hash(COMPONENT).to_hex().as_str(),
        COMPONENT_BLAKE3,
        "fixture changed: re-pin the golden constants"
    );
}

#[test]
fn describe_reports_manifest_seekable() {
    let (host, transform) = shared();
    let d = host.describe(transform, "recreate").expect("describe");
    assert_eq!(d.seek, SeekClass::ManifestSeekable);
    assert!(d.random_access_inputs.is_empty());
}

/// wasm recreate == native construction, twice (determinism).
#[test]
fn recreate_matches_native_and_is_deterministic() {
    let (image, layout, stripped) = vectors();
    let (host, transform) = shared();
    for _ in 0..2 {
        let out = Collector::default();
        host.run(
            transform,
            "recreate",
            &[],
            vec![sequential(layout.clone()), sequential(stripped.clone())],
            vec![Box::new(out.clone())],
        )
        .expect("recreate succeeds");
        assert_eq!(out.take(), image, "wasm output equals native image");
    }
}

/// D49 seek-equivalence across run boundaries, sector boundaries, the
/// literal region, and EOF.
#[test]
fn served_ranges_match_materialization() {
    let (image, layout, stripped) = vectors();
    let (host, transform) = shared();
    let total = image.len() as u64;
    let sector = SECTOR as u64;
    for (offset, len) in [
        (0u64, 1u64),
        (sector - 1, 2),        // sector straddle inside mode-1 run
        (5 * sector - 3, 3006), // run boundary into the literal junk
        (5 * sector + 2999, 2), // junk -> mode 2 form 1 boundary
        (total - 10, 100),      // EOF clamp inside trailing literal
        (total + 7, 4),         // fully past EOF: empty
        (0, total),             // the whole image through serve-range
    ] {
        let out = Collector::default();
        let inputs: Vec<Box<dyn RangeRead>> =
            vec![Box::new(layout.clone()), Box::new(stripped.clone())];
        host.serve_range(
            transform,
            "recreate",
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
fn malformed_layout_and_short_stripped_are_guest_errors() {
    let (_, layout, stripped) = vectors();
    let (host, transform) = shared();
    // Truncated stripped input.
    let err = host
        .run(
            transform,
            "recreate",
            &[],
            vec![
                sequential(layout.clone()),
                sequential(stripped[..stripped.len() - 9].to_vec()),
            ],
            vec![Box::new(Collector::default())],
        )
        .expect_err("must fail");
    assert!(matches!(err, RuntimeError::Transform(_)), "{err:?}");
    // Layout that under-covers the stripped input.
    let err = host
        .run(
            transform,
            "recreate",
            &[],
            vec![
                sequential(encode_record(LayoutRecord { kind: 0, count: 4 }).to_vec()),
                sequential(stripped.clone()),
            ],
            vec![Box::new(Collector::default())],
        )
        .expect_err("must fail");
    assert!(
        matches!(err, RuntimeError::Transform(ref m) if m.contains("covers")),
        "{err:?}"
    );
    // Unknown record kind.
    let err = host
        .run(
            transform,
            "recreate",
            &[],
            vec![sequential(vec![7, 1, 0, 0, 0]), sequential(stripped)],
            vec![Box::new(Collector::default())],
        )
        .expect_err("must fail");
    assert!(
        matches!(err, RuntimeError::Transform(ref m) if m.contains("unknown")),
        "{err:?}"
    );
}
