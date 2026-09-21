//! D128, end to end: an analyzer that PANICS costs one item, not the
//! worker, and the item settles instead of coming back forever.
//!
//! The live shape this reproduces: `preflate-split` runs preflate-rs
//! 0.7.6 natively, and on some of our deflate streams it panics with
//! `index out of bounds` in `tree_predictor.rs:169`. Nothing caught it,
//! so the unwind escaped into the detached drone thread running the
//! sweep and killed it. Four of those inside two seconds took the whole
//! refinement fleet down, permanently and silently — the prime tracks
//! stop flags, not liveness, so it went on believing it had three
//! drones while all three were gone. The daemon then sat at 4.6% CPU
//! doing nothing until the next restart, which is how `chd-verify`
//! reached zero analyses behind a queue of 654,489 items.
//!
//! D126's trap is the neighbouring case and settles differently: there
//! the analyzer never reached the bytes, so the item waits (D116). Here
//! it did reach them and its own code fell over, which under D81 is a
//! deterministic conclusion and must settle.
//!
//! The real panic needs a corpus this test does not have, so the
//! analyzer panics directly. That substitution is honest for what is
//! under test: everything from the unwind outward is the same code, and
//! the only contribution of the real bug is that it unwinds.

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Namespace as IndexNs, Residency, SweepItem};
use datboi_ingest::refine::{
    AnalysisResult, AnalyzeError, Analyzer, AnalyzerClass, Logical, Pulse, run_sweep,
};
use datboi_store_fs::{Namespace as StoreNs, Store};

/// Panics the way preflate-rs does: from inside `analyze`, on the
/// calling thread, with a message worth keeping.
struct PanickingAnalyzer;

impl Analyzer for PanickingAnalyzer {
    fn name(&self) -> &'static str {
        "panicking-test-analyzer/1"
    }
    fn class(&self) -> AnalyzerClass {
        AnalyzerClass::Fallback
    }
    fn family(&self) -> &'static str {
        "panicking-test-analyzer"
    }
    fn id(&self) -> Blake3 {
        Blake3::compute(self.name().as_bytes())
    }
    fn analyze(
        &mut self,
        _item: &SweepItem,
        _bytes: &Logical<'_, '_>,
        _store: &Store,
        _db: &mut Db,
        _pulse: &mut dyn Pulse,
    ) -> Result<AnalysisResult, AnalyzeError> {
        panic!("index out of bounds: the len is 10 but the index is 10");
    }
}

#[test]
fn a_panicking_analyzer_settles_its_item_and_does_not_escape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    let bytes = b"bytes that make the analyzer fall over";
    let hash = Blake3::compute(bytes);
    store
        .put(StoreNs::Data, hash, bytes.as_slice())
        .expect("put");
    db.upsert_blob(
        &hash,
        Some(bytes.len() as u64),
        IndexNs::Data,
        Residency::Resident,
    )
    .expect("blob row");

    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let logical = Logical::new(&store, &exec);

    // 1. The sweep RETURNS. Before containment this unwound into the
    //    caller, which for the daemon was a detached drone thread.
    let report = run_sweep(&mut db, &store, &logical, &mut PanickingAnalyzer, 50)
        .expect("a panicking analyzer must not fail the sweep");

    // 2. One item, settled Negative, surfaced separately so an operator
    //    sees a panic and not an ordinary negative result.
    assert_eq!(report.analyzed, 1, "the item was accounted for");
    assert_eq!(report.negative, 1, "D81: a deterministic verdict settles");
    assert_eq!(report.trapped.len(), 1, "surfaced as a panic");
    assert_eq!(report.trapped[0].0, hash);
    assert!(
        report.trapped[0].1.contains("index out of bounds"),
        "the payload is kept — it is the only clue to the upstream bug, got: {}",
        report.trapped[0].1
    );
    assert!(
        report.errors.is_empty(),
        "not environmental: retrying this forever is the bug being fixed"
    );

    // 3. It does not come back. That is what settling buys over
    //    deferring: the queue row is gone and the next pass has nothing
    //    to claim.
    let queued = db
        .sweep_queue_len(&PanickingAnalyzer.id())
        .expect("queue len");
    assert_eq!(queued, 0, "the claim was released and the row dropped");

    let again =
        run_sweep(&mut db, &store, &logical, &mut PanickingAnalyzer, 50).expect("second sweep");
    assert_eq!(
        again.analyzed, 0,
        "a settled item is not re-claimed on the next pass"
    );
}
