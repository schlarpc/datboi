//! D54 gate: components carry their identity in-band, and anonymous
//! ones don't load. `unstamped.wasm` is the pre-D54 reference-stream
//! build, kept exactly for this refusal test.

use datboi_runtime::attribution::parse_attribution;
use datboi_runtime::stream::StreamHost;
use datboi_runtime::{Limits, RuntimeError};

const STAMPED: &[u8] = include_bytes!("fixtures/xf_reference_stream.wasm");
const UNSTAMPED: &[u8] = include_bytes!("fixtures/unstamped.wasm");

#[test]
fn stamped_component_parses_the_required_set() {
    let a = parse_attribution(STAMPED).expect("stamped");
    assert_eq!(a.name, "datboi:xf-reference-stream");
    assert!(a.description.contains("Streaming reference transform"));
    assert!(a.source.contains("github.com/schlarpc/datboi"));
    assert!(a.revision.starts_with("tree:"), "{}", a.revision);
}

#[test]
fn anonymous_component_is_refused_at_load() {
    let host = StreamHost::new(Limits::default()).expect("host");
    let err = match host.load(UNSTAMPED) {
        Err(e) => e,
        Ok(_) => panic!("anonymous must not load"),
    };
    assert!(
        matches!(err, RuntimeError::Component(ref e)
            if e.to_string().contains("attribution")),
        "{err:?}"
    );
    // The parse names every missing field.
    let msg = parse_attribution(UNSTAMPED).expect_err("missing set");
    for field in ["name", "description", "source", "revision"] {
        assert!(msg.contains(field), "{msg}");
    }
}

#[test]
fn garbage_is_not_a_component() {
    assert!(parse_attribution(b"not wasm at all").is_err());
}
