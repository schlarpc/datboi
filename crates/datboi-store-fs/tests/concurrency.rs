//! Concurrent-ingest invariant (runs under plain `cargo test` — the fast
//! variant the flake gate exercises every commit). Two writers racing the
//! same bytes must converge on exactly one valid blob, never a panic or a
//! corrupt/partial file. This is the same tmp→rename protocol the crash
//! harness stresses, checked under thread contention instead of process
//! death.

use std::sync::Arc;
use std::thread;

use datboi_store_fs::store::PutOutcome;
use datboi_store_fs::{Namespace, Store};

#[test]
fn concurrent_put_new_same_bytes_converges_to_one_valid_blob() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(dir.path().join("store")).expect("open"));
    let data: Arc<[u8]> = Arc::from(vec![0x7c; 256 * 1024].into_boxed_slice());

    let threads: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            let data = Arc::clone(&data);
            thread::spawn(move || {
                // No call may panic or error; outcome is Stored or
                // AlreadyPresent (renaming an identical temp over an existing
                // blob is still correct, so we don't over-constrain which).
                let (hash, _aliases, outcome) =
                    store.put_new(Namespace::Data, &*data).expect("put_new");
                assert!(matches!(
                    outcome,
                    PutOutcome::Stored | PutOutcome::AlreadyPresent
                ));
                hash
            })
        })
        .collect();

    let hashes: Vec<_> = threads
        .into_iter()
        .map(|t| t.join().expect("thread"))
        .collect();
    // Same bytes ⇒ same identity from every racer.
    assert!(hashes.iter().all(|h| *h == hashes[0]));

    // Exactly one blob on disk, and it verifies.
    let listed: Vec<_> = store
        .list(Namespace::Data)
        .map(|r| r.expect("no foreign/partial files after a race"))
        .collect();
    assert_eq!(listed.len(), 1, "one blob, not one-per-racer");
    assert_eq!(
        store.verify(Namespace::Data, &listed[0].0).expect("verify"),
        datboi_store_fs::store::VerifyOutcome::Valid
    );
}
