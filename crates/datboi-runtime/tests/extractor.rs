//! Conformance gate for the `datboi:extractor@1` component (ex-unrar,
//! D58 pathfinder, D89 shape).
//!
//! Exercises the committed component under the real wasmtime host:
//!   * `version.rar` enumerates + extracts byte-exact against known bytes;
//!   * a corrupt archive refuses the whole thing (no partial member);
//!   * determinism — same input twice yields identical output bytes, and
//!     the D89 batch clause: member bytes are a pure function of
//!     (containers, ix) regardless of the request set;
//!   * the v1 policy cuts refuse as policy, not ABI: multi-archive lists
//!     and non-empty params error cleanly.

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

/// The committed, stamped component (D5/D6/D54) recipes will pin.
/// The nix-built `ex-unrar` component (D66), embedded at compile time
/// via `DATBOI_COMPONENTS_DIR` (build.rs re-exports it) — never a
/// checked-in artifact.
const EX_UNRAR: &[u8] = include_bytes!(concat!(
    env!("DATBOI_COMPONENTS_DIR"),
    "/datboi_ex_unrar.wasm"
));

/// The rar test fixture (one member "VERSION" = "unrar-0.4.0"). rar cannot
/// be created programmatically (extraction-only), hence the committed blob.
const VERSION_RAR: &[u8] = include_bytes!("../../datboi-ingest/tests/fixtures/version.rar");
const VERSION_BYTES: &[u8] = b"unrar-0.4.0";

fn host() -> ExtractorHost {
    ExtractorHost::new(Limits::default()).expect("host")
}

fn archive() -> Vec<Box<dyn RangeRead>> {
    vec![Box::new(VERSION_RAR.to_vec()) as Box<dyn RangeRead>]
}

#[test]
fn enumerate_lists_the_member() {
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
    let members = host
        .enumerate(&component, archive(), &[])
        .expect("enumerate");
    assert_eq!(members.len(), 1, "one file member");
    let m = &members[0];
    assert_eq!(m.ix, 0);
    assert_eq!(m.name, "VERSION");
    assert_eq!(m.size, VERSION_BYTES.len() as u64);
    assert!(!m.solid);
}

#[test]
fn extract_is_byte_exact() {
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
    let out = SharedBuf::default();
    host.extract(&component, archive(), &[], vec![(0, Box::new(out.clone()))])
        .expect("extract");
    assert_eq!(out.take(), VERSION_BYTES, "member bytes bit-exact");
}

#[test]
fn extraction_is_deterministic() {
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
    let run = || {
        let out = SharedBuf::default();
        host.extract(&component, archive(), &[], vec![(0, Box::new(out.clone()))])
            .expect("extract");
        out.take()
    };
    assert_eq!(run(), run(), "same input → identical output bytes");
}

#[test]
fn empty_batch_is_a_no_op() {
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
    host.extract(&component, archive(), &[], Vec::new())
        .expect("empty batch succeeds trivially");
}

#[test]
fn duplicate_request_ix_refuses() {
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
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
    // list<file> is ABI (D89); multi-volume support is this component's
    // POLICY cut — a two-container call must refuse cleanly.
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
    let archives: Vec<Box<dyn RangeRead>> = vec![
        Box::new(VERSION_RAR.to_vec()),
        Box::new(VERSION_RAR.to_vec()),
    ];
    let err = host
        .enumerate(&component, archives, &[])
        .expect_err("multi-volume is a v1 scope cut");
    assert!(format!("{err}").contains("multi-volume"), "{err}");
}

#[test]
fn nonempty_params_refuse() {
    // Params are recipe content: a component must refuse params it does
    // not understand, never ignore them.
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
    let err = host
        .enumerate(&component, archive(), &[0xa0])
        .expect_err("ex-unrar takes no params");
    assert!(format!("{err}").contains("params"), "{err}");
}

#[test]
fn out_of_range_member_refuses() {
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
    let out = SharedBuf::default();
    let err = host
        .extract(
            &component,
            archive(),
            &[],
            vec![(7, Box::new(out.clone()))], // only member 0 exists
        )
        .expect_err("out-of-range member must fail");
    assert!(out.take().is_empty(), "no bytes produced on refusal");
    let _ = err;
}

#[test]
fn corrupt_archive_refuses_whole() {
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
    // A valid rar signature followed by garbage: the header walk must fail
    // (trap or error), never surface bytes.
    let mut bytes = b"Rar!\x1a\x07\x00".to_vec();
    bytes.extend_from_slice(&[0xFFu8; 64]);
    let result = host.enumerate(&component, vec![Box::new(bytes)], &[]);
    assert!(result.is_err(), "corrupt archive must be refused");
}
