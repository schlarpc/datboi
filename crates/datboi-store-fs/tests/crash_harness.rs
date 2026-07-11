//! Crash-consistency harness (built only with `--features crash-injection`).
//!
//! Proves the store's core invariant survives a `kill -9` at any point in
//! the publish protocol: **no partial blob is ever visible under data/ —
//! every blob `list()` yields hash-verifies, and interrupted writes leave
//! only collectable temp files.** Two strategies:
//!
//! * `injected_phase_*` — the child aborts (SIGABRT, uncatchable, no flush)
//!   at each named step, giving deterministic coverage of every window.
//! * `sigkill_at_random_points` — the child writes in a loop and the parent
//!   SIGKILLs it at arbitrary wall-clock moments, catching windows between
//!   the labelled phases.
//!
//! What it does NOT prove: durability across power loss. `fsync`'s guarantee
//! (bytes are on stable media) can't be observed without pulling power; we
//! test crash-*consistency* of the rename protocol — a process dying never
//! exposes a torn blob — and rely on `fsync` for the durability half. The
//! model is exact for process death (SIGKILL/abort): `write()` reaches the
//! page cache immediately, so the OS holds whatever was written, and a blob
//! becomes visible only via the atomic post-fsync `rename`.
#![cfg(feature = "crash-injection")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use datboi_store_fs::store::VerifyOutcome;
use datboi_store_fs::{Namespace, Store};

const CHILD: &str = env!("CARGO_BIN_EXE_datboi-crash-child");
const BLOB_SIZE: usize = 512 * 1024;

fn fresh_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("store");
    (dir, root)
}

/// The invariant: everything `list()` surfaces is a complete, verifying
/// blob — no foreign or partial files leak into the namespace trees.
fn assert_listed_blobs_all_valid(root: &Path) {
    let store = Store::open(root).expect("reopen store");
    for ns in [Namespace::Data, Namespace::Meta] {
        for item in store.list(ns) {
            let (hash, _size) = item.expect("no foreign/partial file in the namespace tree");
            assert_eq!(
                store.verify(ns, &hash).expect("verify"),
                VerifyOutcome::Valid,
                "a listed blob failed to verify — a torn write became visible"
            );
        }
    }
}

fn data_blob_count(root: &Path) -> usize {
    let store = Store::open(root).expect("reopen store");
    store.list(Namespace::Data).count()
}

fn temp_files(root: &Path) -> Vec<PathBuf> {
    let tmp = root.join("tmp");
    let Ok(rd) = fs::read_dir(&tmp) else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "temp"))
        .collect()
}

/// After any crash the store must still accept new writes and cleanup must
/// reclaim orphaned temps.
fn assert_store_recovers(root: &Path) {
    let store = Store::open(root).expect("reopen store");
    let (hash, _, _) = store
        .put_new(Namespace::Data, &b"post-crash write"[..])
        .expect("store still writable after crash");
    assert_eq!(
        store.verify(Namespace::Data, &hash).expect("verify"),
        VerifyOutcome::Valid
    );
    // Orphaned temps from the crash are collectable (zero-age sweep).
    store.cleanup_temp(Duration::ZERO).expect("cleanup");
    assert!(
        temp_files(root).is_empty(),
        "temps not collectable after crash"
    );
}

fn run_child_single(root: &Path, dir: &Path, phase: &str, at_bytes: Option<usize>) {
    let mut cmd = Command::new(CHILD);
    cmd.arg("single")
        .current_dir(dir) // any core dump lands in the tempdir
        .env("DATBOI_STORE_ROOT", root)
        .env("DATBOI_BLOB_SIZE", BLOB_SIZE.to_string())
        .env("DATBOI_CRASH_PHASE", phase);
    if let Some(at) = at_bytes {
        cmd.env("DATBOI_CRASH_AT_BYTES", at.to_string());
    }
    let status = cmd.status().expect("spawn datboi-crash-child");
    assert!(
        !status.success(),
        "child was supposed to abort at phase {phase}, but exited cleanly"
    );
}

#[test]
fn injected_phase_before_rename_publishes_nothing() {
    // Every step up to (not including) the rename: the blob must be invisible
    // and a temp orphan must remain.
    let cases: &[(&str, Option<usize>)] = &[
        ("after-temp-create", None),
        ("mid-write", Some(BLOB_SIZE / 4)),
        ("after-write", None),
        ("after-fsync", None),
    ];
    for &(phase, at) in cases {
        let (dir, root) = fresh_root();
        run_child_single(&root, dir.path(), phase, at);

        assert_eq!(
            data_blob_count(&root),
            0,
            "{phase}: a blob became visible pre-rename"
        );
        assert!(
            !temp_files(&root).is_empty(),
            "{phase}: expected a temp orphan from the interrupted write"
        );
        assert_listed_blobs_all_valid(&root);
        assert_store_recovers(&root);
    }
}

#[test]
fn injected_phase_after_rename_exposes_one_valid_blob() {
    // The rename is atomic: once it runs, the (complete) blob is visible even
    // though the durability-fsync of the directory never happened.
    let (dir, root) = fresh_root();
    run_child_single(&root, dir.path(), "after-rename", None);

    assert_eq!(
        data_blob_count(&root),
        1,
        "the renamed blob should be visible"
    );
    assert_listed_blobs_all_valid(&root);
    assert!(
        temp_files(&root).is_empty(),
        "rename should have consumed the temp"
    );
    assert_store_recovers(&root);
}

#[test]
fn sigkill_at_random_points_never_corrupts() {
    // Kill the writer at arbitrary real moments; whatever landed must be
    // consistent. Repeated with varying delays to sweep the timing space.
    for i in 0..10u64 {
        let (dir, root) = fresh_root();
        let mut child = Command::new(CHILD)
            .arg("loop")
            .current_dir(dir.path())
            .env("DATBOI_STORE_ROOT", &root)
            .env("DATBOI_BLOB_SIZE", BLOB_SIZE.to_string())
            .spawn()
            .expect("spawn datboi-crash-child loop");

        // Vary the kill instant without a rand dependency: mix the loop index
        // with the wall clock's sub-millisecond jitter.
        let jitter = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::from(d.subsec_nanos()) % 4000)
            .unwrap_or(0);
        std::thread::sleep(Duration::from_micros(600 + i * 900 + jitter));

        child.kill().expect("SIGKILL child");
        let status = child.wait().expect("reap child");
        assert!(!status.success(), "a SIGKILLed process never exits cleanly");

        assert_listed_blobs_all_valid(&root);
        // Store is still usable and its temps are collectable.
        assert_store_recovers(&root);
    }
}
