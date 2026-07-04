//! The M1 store: complete blobs only (D14 staging), no partial states, no
//! eviction — no code path here can destroy verified bytes (D35).
//!
//! Durability discipline (docs/10-cas.md): stream to `tmp/<unique>` while
//! hashing, verify against the expected hash, `fsync` the file, atomically
//! `rename()` into the sharded final path, then `fsync` the parent
//! directory so the rename itself is durable. Shard directories are
//! created on demand; their creation is made durable by the same
//! parent-dir fsync on first publish into them. `rename()` is atomic on
//! NFS; no locks are taken (single-writer daemon owns the tree).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use datboi_core::alias::{AliasHasher, AliasTuple};
use datboi_core::hash::Blake3;

use crate::layout::{self, Namespace};

/// Streaming buffer size; nothing in this crate buffers more than this.
const CHUNK: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The streamed bytes do not hash to what the caller claimed. The temp
    /// file has been deleted; nothing was published.
    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: Blake3, actual: Blake3 },
    /// A file in the shard tree that is neither a blob nor a known sidecar
    /// (recovery scans surface these instead of silently skipping).
    #[error("foreign file in store tree: {path}")]
    Foreign { path: PathBuf },
}

impl StoreError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The blob was written and durably published by this call.
    Stored,
    /// An identical blob was already present (same hash ⇒ same bytes).
    AlreadyPresent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyOutcome {
    Valid,
    /// On-disk bytes no longer hash to the name (logical corruption; ZFS
    /// scrubs bitrot below us, this catches everything else).
    Corrupt {
        actual: Blake3,
    },
    Missing,
}

pub struct Store {
    root: PathBuf,
    /// Per-open token making temp names unique across restarts that reuse
    /// a pid; combined with a counter for uniqueness within the process.
    temp_token: u64,
    temp_counter: AtomicU64,
}

impl Store {
    /// Open (creating if needed) a store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        for dir in [
            root.clone(),
            root.join(Namespace::Data.dir()),
            root.join(Namespace::Meta.dir()),
            root.join("tmp"),
        ] {
            fs::create_dir_all(&dir).map_err(|e| StoreError::io(&dir, e))?;
        }
        fsync_dir(&root)?;
        Ok(Self {
            root,
            temp_token: entropy_token(),
            temp_counter: AtomicU64::new(0),
        })
    }

    /// Stream `reader` into the store, verifying it hashes to `expected`.
    /// On mismatch nothing is published and the temp file is removed.
    pub fn put(
        &self,
        ns: Namespace,
        expected: Blake3,
        reader: impl Read,
    ) -> Result<PutOutcome, StoreError> {
        let (outcome, _) = self.put_inner(ns, expected, reader, false)?;
        Ok(outcome)
    }

    /// Like [`Store::put`], but computes the full alias tuple
    /// (crc32/md5/sha1/sha256 + size) in the same single pass — the ingest
    /// fast path (D2: one streaming pass computes everything).
    ///
    /// Note: when the blob is already present the reader is still fully
    /// consumed (the aliases have to come from somewhere).
    pub fn put_with_aliases(
        &self,
        ns: Namespace,
        expected: Blake3,
        reader: impl Read,
    ) -> Result<(PutOutcome, AliasTuple), StoreError> {
        let (outcome, aliases) = self.put_inner(ns, expected, reader, true)?;
        Ok((outcome, aliases.expect("aliases requested")))
    }

    /// Stream `reader` into the store without knowing its hash up front —
    /// the ingest entry point (D40 `--copy`): one pass computes the full
    /// alias tuple, names the blob by its computed blake3, and publishes
    /// with the same temp/fsync/rename discipline as [`Store::put`]. There
    /// is no expectation to violate, so the only failure mode is I/O; a
    /// mid-read error deletes the temp and publishes nothing.
    pub fn put_new(
        &self,
        ns: Namespace,
        mut reader: impl Read,
    ) -> Result<(Blake3, AliasTuple, PutOutcome), StoreError> {
        let temp = self.new_temp_path();
        let mut file = File::create_new(&temp).map_err(|e| StoreError::io(&temp, e))?;
        let mut hasher = AliasHasher::new();
        let mut buf = vec![0u8; CHUNK];
        let result = loop {
            match reader.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    hasher.update(&buf[..n]);
                    if let Err(e) = file.write_all(&buf[..n]) {
                        break Err(StoreError::io(&temp, e));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => break Err(StoreError::io(&temp, e)),
            }
        };
        if let Err(e) = result {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(e);
        }

        let aliases = hasher.finalize();
        let hash = aliases.blake3;
        let final_path = self.blob_path(ns, &hash);
        if final_path.exists() {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Ok((hash, aliases, PutOutcome::AlreadyPresent));
        }

        file.sync_all().map_err(|e| StoreError::io(&temp, e))?;
        drop(file);
        let parent = final_path.parent().expect("blob paths have parents");
        fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
        fs::rename(&temp, &final_path).map_err(|e| StoreError::io(&final_path, e))?;
        fsync_dir(parent)?;
        Ok((hash, aliases, PutOutcome::Stored))
    }

    fn put_inner(
        &self,
        ns: Namespace,
        expected: Blake3,
        mut reader: impl Read,
        want_aliases: bool,
    ) -> Result<(PutOutcome, Option<AliasTuple>), StoreError> {
        let final_path = self.blob_path(ns, &expected);
        // Fast path: identical bytes are already published. Without the
        // alias request we can skip reading entirely.
        if !want_aliases && final_path.exists() {
            return Ok((PutOutcome::AlreadyPresent, None));
        }

        let temp = self.new_temp_path();
        let mut file = File::create_new(&temp).map_err(|e| StoreError::io(&temp, e))?;
        let mut hasher = PutHasher::new(want_aliases);
        let mut buf = vec![0u8; CHUNK];
        let result = loop {
            match reader.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    hasher.update(&buf[..n]);
                    if let Err(e) = file.write_all(&buf[..n]) {
                        break Err(StoreError::io(&temp, e));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => break Err(StoreError::io(&temp, e)),
            }
        };
        if let Err(e) = result {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(e);
        }

        let (actual, aliases) = hasher.finalize();
        if actual != expected {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(StoreError::HashMismatch { expected, actual });
        }

        // Concurrent identical put: renaming over the existing file would
        // also be correct (same bytes), but detecting it keeps the outcome
        // honest and skips a durable rename.
        if final_path.exists() {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Ok((PutOutcome::AlreadyPresent, aliases));
        }

        file.sync_all().map_err(|e| StoreError::io(&temp, e))?;
        drop(file);
        let parent = final_path.parent().expect("blob paths have parents");
        fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
        fs::rename(&temp, &final_path).map_err(|e| StoreError::io(&final_path, e))?;
        fsync_dir(parent)?;
        Ok((PutOutcome::Stored, aliases))
    }

    /// Open a blob for reading; `None` if absent.
    pub fn get(&self, ns: Namespace, hash: &Blake3) -> Result<Option<File>, StoreError> {
        let path = self.blob_path(ns, hash);
        match File::open(&path) {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::io(&path, e)),
        }
    }

    pub fn has(&self, ns: Namespace, hash: &Blake3) -> bool {
        self.blob_path(ns, hash).exists()
    }

    pub fn len(&self, ns: Namespace, hash: &Blake3) -> Result<Option<u64>, StoreError> {
        let path = self.blob_path(ns, hash);
        match fs::metadata(&path) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::io(&path, e)),
        }
    }

    /// Walk one namespace's shard tree — the recovery-scan primitive
    /// (D15). Yields `(hash, size)` per blob file; foreign files come back
    /// as [`StoreError::Foreign`] items so callers can count them without
    /// aborting the scan. `.obao` sidecars are expected store files and
    /// are skipped silently.
    pub fn list(&self, ns: Namespace) -> ListIter {
        let root = self.root.join(ns.dir());
        let mut stack = Vec::new();
        match fs::read_dir(&root) {
            Ok(rd) => stack.push(rd),
            Err(e) => return ListIter::failed(StoreError::io(&root, e)),
        }
        ListIter {
            stack,
            pending_error: None,
        }
    }

    /// Re-hash one blob (scrub primitive).
    pub fn verify(&self, ns: Namespace, hash: &Blake3) -> Result<VerifyOutcome, StoreError> {
        let Some(mut file) = self.get(ns, hash)? else {
            return Ok(VerifyOutcome::Missing);
        };
        let path = self.blob_path(ns, hash);
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buf[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(StoreError::io(&path, e)),
            }
        }
        let actual = Blake3(*hasher.finalize().as_bytes());
        if actual == *hash {
            Ok(VerifyOutcome::Valid)
        } else {
            Ok(VerifyOutcome::Corrupt { actual })
        }
    }

    /// Remove crash-orphaned temp files older than `max_age`. Returns how
    /// many were removed. Never touches published blobs.
    pub fn cleanup_temp(&self, max_age: Duration) -> Result<usize, StoreError> {
        let tmp = self.root.join("tmp");
        let cutoff = SystemTime::now().checked_sub(max_age);
        let mut removed = 0;
        for entry in fs::read_dir(&tmp).map_err(|e| StoreError::io(&tmp, e))? {
            let entry = entry.map_err(|e| StoreError::io(&tmp, e))?;
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue; // vanished or unreadable: someone else's problem
            };
            if !meta.is_file() {
                continue;
            }
            let stale = match (cutoff, meta.modified()) {
                (Some(cutoff), Ok(mtime)) => mtime <= cutoff,
                // Unknown age: treat as stale only for the "remove
                // everything" case (max_age zero).
                _ => max_age.is_zero(),
            };
            if stale && fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn blob_path(&self, ns: Namespace, hash: &Blake3) -> PathBuf {
        self.root.join(layout::blob_path(ns, hash))
    }

    fn new_temp_path(&self) -> PathBuf {
        let n = self.temp_counter.fetch_add(1, Ordering::Relaxed);
        self.root.join("tmp").join(format!(
            "{:08x}-{:016x}-{n:x}.temp",
            std::process::id(),
            self.temp_token,
        ))
    }
}

/// blake3-only or full-alias hashing behind one seam so `put` and
/// `put_with_aliases` share the streaming loop. Boxed: hasher states are
/// kilobytes and one exists per in-flight put.
enum PutHasher {
    Blake3(Box<blake3::Hasher>),
    Alias(Box<AliasHasher>),
}

impl PutHasher {
    fn new(want_aliases: bool) -> Self {
        if want_aliases {
            Self::Alias(Box::new(AliasHasher::new()))
        } else {
            Self::Blake3(Box::new(blake3::Hasher::new()))
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Blake3(h) => {
                h.update(bytes);
            }
            Self::Alias(h) => h.update(bytes),
        }
    }

    fn finalize(self) -> (Blake3, Option<AliasTuple>) {
        match self {
            Self::Blake3(h) => (Blake3(*h.finalize().as_bytes()), None),
            Self::Alias(h) => {
                let tuple = h.finalize();
                (tuple.blake3, Some(tuple))
            }
        }
    }
}

/// Depth-first walk over a namespace's shard tree. Directory read errors
/// are yielded as items and that directory is skipped; iteration continues.
pub struct ListIter {
    stack: Vec<fs::ReadDir>,
    pending_error: Option<StoreError>,
}

impl ListIter {
    fn failed(err: StoreError) -> Self {
        Self {
            stack: Vec::new(),
            pending_error: Some(err),
        }
    }
}

impl Iterator for ListIter {
    type Item = Result<(Blake3, u64), StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(err) = self.pending_error.take() {
            return Some(Err(err));
        }
        loop {
            let rd = self.stack.last_mut()?;
            let Some(entry) = rd.next() else {
                self.stack.pop();
                continue;
            };
            let entry = match entry {
                Ok(e) => e,
                Err(e) => return Some(Err(StoreError::io(Path::new("<readdir>"), e))),
            };
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => match fs::read_dir(&path) {
                    Ok(rd) => self.stack.push(rd),
                    Err(e) => return Some(Err(StoreError::io(&path, e))),
                },
                Ok(ft) if ft.is_file() => match classify(&path) {
                    FileKind::Blob(hash) => {
                        let size = match entry.metadata() {
                            Ok(m) => m.len(),
                            Err(e) => return Some(Err(StoreError::io(&path, e))),
                        };
                        return Some(Ok((hash, size)));
                    }
                    FileKind::Sidecar => {}
                    FileKind::Foreign => return Some(Err(StoreError::Foreign { path })),
                },
                // Symlinks and exotica are foreign by definition.
                Ok(_) => return Some(Err(StoreError::Foreign { path })),
                Err(e) => return Some(Err(StoreError::io(&path, e))),
            }
        }
    }
}

enum FileKind {
    Blob(Blake3),
    Sidecar,
    Foreign,
}

fn classify(path: &Path) -> FileKind {
    let (Some(stem), Some(ext)) = (
        path.file_stem().and_then(|s| s.to_str()),
        path.extension().and_then(|s| s.to_str()),
    ) else {
        return FileKind::Foreign;
    };
    match ext {
        "data" => stem
            .parse::<Blake3>()
            .map_or(FileKind::Foreign, FileKind::Blob),
        "obao" if stem.parse::<Blake3>().is_ok() => FileKind::Sidecar,
        _ => FileKind::Foreign,
    }
}

fn fsync_dir(dir: &Path) -> Result<(), StoreError> {
    let f = File::open(dir).map_err(|e| StoreError::io(dir, e))?;
    f.sync_all().map_err(|e| StoreError::io(dir, e))
}

/// Best-effort per-open entropy without a rand dependency: hashes the
/// randomly-seeded `RandomState` over the current time.
fn entropy_token() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    h.write_u128(now.as_nanos());
    h.finish()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().join("store")).expect("open");
        (dir, store)
    }

    fn tree_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_owned()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).expect("read_dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    #[test]
    fn round_trips_empty_and_multi_chunk_blobs() {
        let (_dir, store) = temp_store();
        for data in [Vec::new(), vec![0xAB; 3 * CHUNK + 17]] {
            let hash = Blake3::compute(&data);
            let outcome = store
                .put(Namespace::Data, hash, data.as_slice())
                .expect("put");
            assert_eq!(outcome, PutOutcome::Stored);
            let mut read_back = Vec::new();
            store
                .get(Namespace::Data, &hash)
                .expect("get")
                .expect("present")
                .read_to_end(&mut read_back)
                .expect("read");
            assert_eq!(read_back, data);
            assert_eq!(
                store.len(Namespace::Data, &hash).expect("len"),
                Some(data.len() as u64)
            );
            assert_eq!(
                store.verify(Namespace::Data, &hash).expect("verify"),
                VerifyOutcome::Valid
            );
        }
    }

    #[test]
    fn wrong_expected_hash_leaves_no_trace() {
        let (_dir, store) = temp_store();
        let wrong = Blake3::compute(b"something else");
        let err = store
            .put(Namespace::Data, wrong, &b"actual bytes"[..])
            .expect_err("must fail");
        let StoreError::HashMismatch { expected, actual } = err else {
            panic!("wrong error: {err}");
        };
        assert_eq!(expected, wrong);
        assert_eq!(actual, Blake3::compute(b"actual bytes"));
        // Nothing anywhere: no temp orphan, no published file.
        assert!(tree_files(&store.root).is_empty());
    }

    #[test]
    fn double_put_is_idempotent() {
        let (_dir, store) = temp_store();
        let hash = Blake3::compute(b"twice");
        assert_eq!(
            store
                .put(Namespace::Data, hash, &b"twice"[..])
                .expect("first"),
            PutOutcome::Stored
        );
        assert_eq!(
            store
                .put(Namespace::Data, hash, &b"twice"[..])
                .expect("second"),
            PutOutcome::AlreadyPresent
        );
        assert_eq!(tree_files(&store.root).len(), 1);
    }

    #[test]
    fn put_with_aliases_matches_direct_hashing() {
        let (_dir, store) = temp_store();
        let data = b"alias me";
        let hash = Blake3::compute(data);
        let (outcome, aliases) = store
            .put_with_aliases(Namespace::Data, hash, &data[..])
            .expect("put");
        assert_eq!(outcome, PutOutcome::Stored);
        let mut direct = AliasHasher::new();
        direct.update(data);
        assert_eq!(aliases, direct.finalize());
        // Second call still yields aliases even though the blob exists.
        let (outcome, aliases2) = store
            .put_with_aliases(Namespace::Data, hash, &data[..])
            .expect("re-put");
        assert_eq!(outcome, PutOutcome::AlreadyPresent);
        assert_eq!(aliases2, aliases);
    }

    #[test]
    fn list_reports_blobs_sidecars_and_foreigners() {
        let (_dir, store) = temp_store();
        let mut expected = Vec::new();
        for data in [&b"one"[..], b"two", b"three"] {
            let hash = Blake3::compute(data);
            store.put(Namespace::Data, hash, data).expect("put");
            expected.push((hash, data.len() as u64));
        }
        // A meta blob must not appear in the data listing.
        let meta = Blake3::compute(b"meta");
        store.put(Namespace::Meta, meta, &b"meta"[..]).expect("put");
        // Plant a silent sidecar and a foreign file next to a real blob.
        let shard = store.blob_path(Namespace::Data, &expected[0].0);
        let shard_dir = shard.parent().expect("parent");
        fs::write(shard_dir.join(format!("{}.obao", expected[0].0)), b"tree").expect("obao");
        fs::write(shard_dir.join("notes.txt"), b"?").expect("foreign");

        let mut found = Vec::new();
        let mut foreign = Vec::new();
        for item in store.list(Namespace::Data) {
            match item {
                Ok(pair) => found.push(pair),
                Err(StoreError::Foreign { path }) => foreign.push(path),
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        found.sort();
        expected.sort();
        assert_eq!(found, expected);
        assert_eq!(foreign.len(), 1);
        assert!(foreign[0].ends_with("notes.txt"));
    }

    #[test]
    fn verify_detects_corruption_and_absence() {
        let (_dir, store) = temp_store();
        let hash = Blake3::compute(b"pristine");
        store
            .put(Namespace::Data, hash, &b"pristine"[..])
            .expect("put");
        let path = store.blob_path(Namespace::Data, &hash);
        fs::write(&path, b"tampered").expect("corrupt");
        assert_eq!(
            store.verify(Namespace::Data, &hash).expect("verify"),
            VerifyOutcome::Corrupt {
                actual: Blake3::compute(b"tampered")
            }
        );
        assert_eq!(
            store
                .verify(Namespace::Data, &Blake3::compute(b"never stored"))
                .expect("verify"),
            VerifyOutcome::Missing
        );
    }

    #[test]
    fn cleanup_temp_respects_age() {
        let (_dir, store) = temp_store();
        let tmp = store.root.join("tmp");
        fs::write(tmp.join("orphan.temp"), b"crash leftover").expect("write");
        // Young files survive a bounded-age sweep…
        assert_eq!(
            store
                .cleanup_temp(Duration::from_secs(3600))
                .expect("sweep"),
            0
        );
        assert!(tmp.join("orphan.temp").exists());
        // …and a zero-age sweep removes everything.
        assert_eq!(store.cleanup_temp(Duration::ZERO).expect("sweep"), 1);
        assert!(!tmp.join("orphan.temp").exists());
    }

    #[test]
    fn put_new_names_by_computed_hash() {
        let (_dir, store) = temp_store();
        let data = vec![0x5a; 2 * CHUNK + 3];
        let (hash, aliases, outcome) = store
            .put_new(Namespace::Data, data.as_slice())
            .expect("put_new");
        assert_eq!(outcome, PutOutcome::Stored);
        assert_eq!(hash, Blake3::compute(&data));
        assert_eq!(aliases.size, data.len() as u64);
        assert_eq!(
            store.verify(Namespace::Data, &hash).expect("verify"),
            VerifyOutcome::Valid
        );
        // Same bytes again: already present, aliases still computed.
        let (hash2, aliases2, outcome2) = store
            .put_new(Namespace::Data, data.as_slice())
            .expect("re-put");
        assert_eq!((hash2, outcome2), (hash, PutOutcome::AlreadyPresent));
        assert_eq!(aliases2, aliases);
        assert_eq!(tree_files(&store.root).len(), 1);
    }

    #[test]
    fn put_new_mid_read_error_leaves_no_trace() {
        struct FailAfter(usize);
        impl Read for FailAfter {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Err(io::Error::other("simulated source failure"));
                }
                let n = self.0.min(buf.len());
                buf[..n].fill(0xEE);
                self.0 -= n;
                Ok(n)
            }
        }
        let (_dir, store) = temp_store();
        store
            .put_new(Namespace::Data, FailAfter(CHUNK + 1))
            .expect_err("source failed");
        assert!(tree_files(&store.root).is_empty());
    }

    proptest! {
        #[test]
        fn round_trip_property(data in prop::collection::vec(any::<u8>(), 0..8192)) {
            let (_dir, store) = temp_store();
            let hash = Blake3::compute(&data);
            store.put(Namespace::Data, hash, data.as_slice()).expect("put");
            let mut read_back = Vec::new();
            store
                .get(Namespace::Data, &hash)
                .expect("get")
                .expect("present")
                .read_to_end(&mut read_back)
                .expect("read");
            prop_assert_eq!(read_back, data);
        }
    }
}
