//! Conformance gate for the `datboi:extractor@1` component (ex-unrar, D58).
//!
//! Exercises the committed component under the real wasmtime host:
//!   * `version.rar` enumerates + extracts byte-exact against known bytes;
//!   * a corrupt archive refuses the whole thing (no partial member);
//!   * determinism — same input twice yields identical output bytes.

use std::io::Write;
use std::sync::{Arc, Mutex};

use datboi_runtime::extractor::ExtractorHost;
use datboi_runtime::Limits;

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
const EX_UNRAR: &[u8] = include_bytes!("../../../transforms/dist/ex_unrar.wasm");

/// The rar test fixture (one member "VERSION" = "unrar-0.4.0"). rar cannot
/// be created programmatically (extraction-only), hence the committed blob.
const VERSION_RAR: &[u8] = include_bytes!("../../datboi-ingest/tests/fixtures/version.rar");
const VERSION_BYTES: &[u8] = b"unrar-0.4.0";

fn host() -> ExtractorHost {
    ExtractorHost::new(Limits::default()).expect("host")
}

#[test]
fn enumerate_lists_the_member() {
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
    let members = host
        .enumerate(&component, Box::new(VERSION_RAR.to_vec()))
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
    host.extract(
        &component,
        Box::new(VERSION_RAR.to_vec()),
        0,
        Box::new(out.clone()),
    )
    .expect("extract");
    assert_eq!(out.take(), VERSION_BYTES, "member bytes bit-exact");
}

#[test]
fn extraction_is_deterministic() {
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
    let run = || {
        let out = SharedBuf::default();
        host.extract(
            &component,
            Box::new(VERSION_RAR.to_vec()),
            0,
            Box::new(out.clone()),
        )
        .expect("extract");
        out.take()
    };
    assert_eq!(run(), run(), "same input → identical output bytes");
}

#[test]
fn out_of_range_member_refuses() {
    let host = host();
    let component = host.load(EX_UNRAR).expect("load");
    let out = SharedBuf::default();
    let err = host
        .extract(
            &component,
            Box::new(VERSION_RAR.to_vec()),
            7, // only member 0 exists
            Box::new(out.clone()),
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
    let result = host.enumerate(&component, Box::new(bytes));
    assert!(result.is_err(), "corrupt archive must be refused");
}
