//! The determinism gate (docs/roadmap.md prototype 3, D5; re-cut for the
//! D89 epoch's single streaming lane).
//!
//! Runs the committed `xf-reference` component — fixed bytes, not rebuilt
//! from source — because that is the production model: recipes pin a
//! component *hash*, so the artifact is the contract, and rebuilding from
//! source would test rustc's reproducibility instead of ours. The golden
//! constants below anchor cross-architecture agreement: this same file runs
//! on x86_64 and aarch64 and must produce identical bytes on both, forever.
//!
//! Since D89 the fixture is also the in-tree proof of the
//! `datboi-guest-transform` BUFFERED sugar: xf-reference authors against
//! blobs-in/blobs-out and the adapter does the streaming, so this gate
//! exercises the vending crate's adapter under the full determinism
//! doctrine.
//!
//! Updating the fixture is a *format event*: new bytes ⇒ new component hash
//! ⇒ the golden constants below must be re-pinned, exactly as recipes in the
//! wild would have to re-pin the component they reference.

use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

use datboi_runtime::stream::{SequentialInput, StreamHost, StreamInput, StreamTransform};
use datboi_runtime::{Limits, RuntimeError, SeekClass};

/// The frozen reference component: `crates/datboi-xf-reference` built for
/// wasm32-unknown-unknown and componentized with `wasm-tools component new`.
const COMPONENT: &[u8] = include_bytes!("fixtures/xf_reference.wasm");

/// blake3 of the fixture itself — the identity a recipe would pin (D5/D6).
const COMPONENT_BLAKE3: &str = "9b9023fb8e5d66f7fdff9f3e7199d5e3172db9bff4c03a779308a6351e65ce94";

/// blake3 of the `bitswap` output over `pattern()` — the cross-arch
/// anchor. Byte semantics carried over the D89 break unchanged, so this
/// constant survived the epoch even though the component hash did not.
const BITSWAP_OUTPUT_BLAKE3: &str =
    "baa41f8f9eba49f83b2d1715f4983f93d1b8ed9abe1cb8ac3d6a08dfd5ce4021";

/// 64 KiB of fixed, incompressible-ish input; covers every byte value.
fn pattern() -> Vec<u8> {
    (0..64 * 1024).map(|i| (i * 31 % 251) as u8).collect()
}

fn host_with(limits: Limits) -> StreamHost {
    StreamHost::new(limits).expect("deterministic config accepted")
}

fn host() -> StreamHost {
    host_with(Limits::default())
}

fn load(host: &StreamHost) -> StreamTransform {
    host.load(COMPONENT).expect("fixture loads")
}

/// Shared Vec sink: the host consumes it as `Box<dyn Write + Send>`, the
/// test keeps a handle to collect the bytes.
#[derive(Default, Clone)]
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

/// Whole-buffer convenience over the streaming host: one sequential
/// input per blob, one collected output.
fn run(host: &StreamHost, op: &str, inputs: &[Vec<u8>]) -> Result<Vec<u8>, RuntimeError> {
    let transform = load(host);
    let out = Collector::default();
    host.run(
        &transform,
        op,
        &[],
        inputs
            .iter()
            .map(|blob| {
                StreamInput::Sequential(SequentialInput {
                    reader: Box::new(Cursor::new(blob.clone())),
                    len: blob.len() as u64,
                })
            })
            .collect(),
        vec![Box::new(out.clone())],
    )?;
    Ok(out.take())
}

#[test]
fn component_bytes_are_pinned() {
    assert_eq!(
        blake3::hash(COMPONENT).to_hex().as_str(),
        COMPONENT_BLAKE3,
        "fixture changed: re-pin the golden constants (this is a format event)"
    );
}

#[test]
fn describe_reports_affine() {
    let host = host();
    let d = host.describe(&load(&host), "bitswap").expect("describe");
    assert_eq!(d.seek, SeekClass::Affine);
    assert!(d.random_access_inputs.is_empty());
}

#[test]
fn unknown_op_describe_refuses_politely() {
    // The D89 error channel: unknown ops refuse, they don't fabricate a
    // descriptor.
    let host = host();
    let err = host
        .describe(&load(&host), "nonsense")
        .expect_err("unknown op must refuse");
    assert!(matches!(err, RuntimeError::Transform(_)), "got {err:?}");
}

#[test]
fn repeated_runs_are_bit_identical() {
    let host = host();
    let input = pattern();
    let first = run(&host, "byteswap", std::slice::from_ref(&input)).expect("run");
    // Independent stores/instances each run; any engine-level nondeterminism
    // (JIT, allocation, layout) would surface as a byte diff here.
    for _ in 0..4 {
        let again = run(&host, "byteswap", std::slice::from_ref(&input)).expect("run");
        assert_eq!(first, again, "two runs of the same recipe diverged");
    }
    // And the output is *correct*, not just stable: byteswap is its own
    // inverse, so applying it to the output must return the input.
    let back = run(&host, "byteswap", std::slice::from_ref(&first)).expect("run");
    assert_eq!(back, input);
}

#[test]
fn golden_output_anchor() {
    let out = run(&host(), "bitswap", &[pattern()]).expect("run");
    assert_eq!(
        blake3::hash(&out).to_hex().as_str(),
        BITSWAP_OUTPUT_BLAKE3,
        "cross-architecture determinism anchor broken"
    );
}

#[test]
fn unknown_op_is_a_transform_error() {
    let err = run(&host(), "nonsense", &[pattern()]).expect_err("unknown op must fail");
    assert!(matches!(err, RuntimeError::Transform(_)), "got {err:?}");
}

#[test]
fn wrong_input_arity_is_a_transform_error() {
    let err =
        run(&host(), "bitswap", &[pattern(), pattern()]).expect_err("swap takes exactly one input");
    assert!(matches!(err, RuntimeError::Transform(_)), "got {err:?}");
}

#[test]
fn fuel_exhaustion_traps_deterministically() {
    let host = host_with(Limits {
        fuel: 10_000, // far too little to swap 64 KiB
        ..Limits::default()
    });
    let err = run(&host, "byteswap", &[pattern()]).expect_err("must trap on fuel exhaustion");
    assert!(matches!(err, RuntimeError::Trap(_)), "got {err:?}");
}

#[test]
fn memory_ceiling_traps_instead_of_ooming() {
    // Enough memory to instantiate, nowhere near enough for the buffered
    // adapter to hold an 8 MiB blob. The guest's failed allocation must
    // become a trap, not host memory growth.
    let host = host_with(Limits {
        memory: 2 << 20,
        ..Limits::default()
    });
    let big = vec![0xAB; 8 << 20];
    let err = run(&host, "byteswap", &[big]).expect_err("must trap at the memory ceiling");
    assert!(matches!(err, RuntimeError::Trap(_)), "got {err:?}");
}

/// Minimal in-test stamp (the attribution sections `wasm-tools metadata
/// add` writes, see attribution.rs) so load() admits the specimen and the
/// LINKER refusal below is what's actually under test.
fn stamp(mut bytes: Vec<u8>) -> Vec<u8> {
    fn leb(mut v: usize, out: &mut Vec<u8>) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }
    fn custom(name: &str, data: &[u8], out: &mut Vec<u8>) {
        let mut payload = Vec::new();
        leb(name.len(), &mut payload);
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(data);
        out.push(0x00);
        leb(payload.len(), out);
        out.extend_from_slice(&payload);
    }
    // component-name: subsection 0 wrapping a name string.
    let mut sub = Vec::new();
    leb("test-specimen".len(), &mut sub);
    sub.extend_from_slice(b"test-specimen");
    let mut name_payload = vec![0x00];
    leb(sub.len(), &mut name_payload);
    name_payload.extend_from_slice(&sub);
    custom("component-name", &name_payload, &mut bytes);
    custom("description", b"gate specimen", &mut bytes);
    custom("source", b"tests/determinism.rs", &mut bytes);
    custom("revision", b"tree:test", &mut bytes);
    bytes
}

#[test]
fn components_with_ambient_imports_cannot_instantiate() {
    // A component demanding a WASI instance: the linker (D5: only the
    // datboi:streams resources, no ambient capabilities) must refuse it
    // outright — this is the enforcement test for "the import surface is
    // the sandbox". Stamped so the D54 load gate isn't what refuses it.
    let wasi_wanting = stamp(
        wat::parse_str(r#"(component (import "wasi:cli/environment@0.2.6" (instance)))"#)
            .expect("valid wat"),
    );
    let host = host();
    let transform = host.load(&wasi_wanting).expect("stamped specimen loads");
    let err = host
        .describe(&transform, "anything")
        .expect_err("ambient import must be unlinkable");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("import") || msg.contains("instantiat"),
        "unexpected failure shape: {msg}"
    );
}
