//! The M1 store: complete blobs only (D14 staging), no partial states, no
//! eviction — no code path here can destroy verified bytes (D35).
//!
//! Durability discipline (docs/cas.md): stream to `tmp/<unique>` while
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

use crate::crash;
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
    /// A verified range read was requested but no outboard sidecar exists
    /// yet (compute one with [`Store::ensure_obao`]).
    #[error("no outboard sidecar for {path}")]
    MissingOutboard { path: PathBuf },
    /// Outboard computation or range validation failed (D49: surfaces as
    /// EIO at serving surfaces, never as bytes).
    #[error("outboard failure at {path}: {source}")]
    Obao {
        path: PathBuf,
        #[source]
        source: crate::obao::ObaoError,
    },
    /// A pack needs at least one member (D91).
    #[error("refusing to write an empty pack")]
    EmptyPack,
    /// A pack member's streamed bytes disagree with its declaration.
    /// The temp pack has been deleted; nothing was published.
    #[error(
        "pack member mismatch: expected {expected} ({expected_len} B), got {got} ({got_len} B)"
    )]
    PackMemberMismatch {
        expected: Blake3,
        got: Blake3,
        expected_len: u64,
        got_len: u64,
    },
    /// A pack file's footer would not parse during a scrub (structural
    /// rot the open-time scan would also refuse). Bytes-are-truth: the
    /// pack fails to resolve, scrub names it.
    #[error("pack footer at {path} is unparseable: {detail}")]
    PackFooter { path: PathBuf, detail: String },
}

impl StoreError {
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
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
    /// D91 pack resolution: packed-member hash → (pack, offset, len),
    /// built from pack footers at open (the footers are the truth; this
    /// map is derivable state). RwLock: reads are the hot path,
    /// `put_pack` the rare writer.
    pub(crate) packs: std::sync::RwLock<std::collections::HashMap<Blake3, crate::pack::PackedLoc>>,
}

impl Store {
    /// Open (creating if needed) a store rooted at `root`. Scans the
    /// pack shard tree (one footer read per pack, D91) to build the
    /// packed-member resolution map; malformed packs are skipped —
    /// their members simply fail to resolve until repaired.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        for dir in [
            root.clone(),
            root.join(Namespace::Data.dir()),
            root.join(Namespace::Meta.dir()),
            root.join("packs"),
            root.join("tmp"),
        ] {
            fs::create_dir_all(&dir).map_err(|e| StoreError::io(&dir, e))?;
        }
        fsync_dir(&root)?;
        let (packs, _bad) = Self::scan_packs(&root);
        Ok(Self {
            root,
            temp_token: entropy_token(),
            temp_counter: AtomicU64::new(0),
            packs: std::sync::RwLock::new(packs),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
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
        crash::inject(crash::Phase::TempCreated);
        let mut hasher = AliasHasher::new();
        let mut buf = vec![0u8; CHUNK];
        let mut written: u64 = 0;
        let result = loop {
            match reader.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    hasher.update(&buf[..n]);
                    if let Err(e) = file.write_all(&buf[..n]) {
                        break Err(StoreError::io(&temp, e));
                    }
                    written += n as u64;
                    crash::inject_mid_write(written);
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

        crash::inject(crash::Phase::Written);
        file.sync_all().map_err(|e| StoreError::io(&temp, e))?;
        drop(file);
        let parent = final_path.parent().expect("blob paths have parents");
        fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
        crash::inject(crash::Phase::Fsynced);
        fs::rename(&temp, &final_path).map_err(|e| StoreError::io(&final_path, e))?;
        crash::inject(crash::Phase::Renamed);
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

    /// Open a blob for reading; `None` if absent. Loose files win;
    /// data-namespace blobs fall through to the D91 pack map, coming
    /// back as bounded windows — callers cannot tell the difference,
    /// which is the design.
    pub fn get(&self, ns: Namespace, hash: &Blake3) -> Result<Option<crate::pack::Blob>, StoreError> {
        let path = self.blob_path(ns, hash);
        match File::open(&path) {
            Ok(f) => return Ok(Some(crate::pack::Blob::loose(f))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(StoreError::io(&path, e)),
        }
        if ns != Namespace::Data {
            return Ok(None);
        }
        let Some(loc) = self.packed_loc(hash) else {
            return Ok(None);
        };
        let pack_path = self.pack_path(&loc.pack);
        let file = File::open(&pack_path).map_err(|e| StoreError::io(&pack_path, e))?;
        let blob = crate::pack::Blob::packed(file, loc.offset, loc.len)
            .map_err(|e| StoreError::io(&pack_path, e))?;
        Ok(Some(blob))
    }

    pub fn has(&self, ns: Namespace, hash: &Blake3) -> bool {
        self.blob_path(ns, hash).exists()
            || (ns == Namespace::Data && self.packed_loc(hash).is_some())
    }

    /// Does a LOOSE file exist for this blob? `has` also answers for
    /// packed members; eviction planners need the distinction (a
    /// packed blob has nothing evictable, D91).
    #[must_use]
    pub fn has_loose(&self, ns: Namespace, hash: &Blake3) -> bool {
        self.blob_path(ns, hash).exists()
    }

    pub fn len(&self, ns: Namespace, hash: &Blake3) -> Result<Option<u64>, StoreError> {
        let path = self.blob_path(ns, hash);
        match fs::metadata(&path) {
            Ok(m) => return Ok(Some(m.len())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(StoreError::io(&path, e)),
        }
        if ns == Namespace::Data
            && let Some(loc) = self.packed_loc(hash)
        {
            return Ok(Some(loc.len));
        }
        Ok(None)
    }

    /// Free bytes on the filesystem holding the store root — the D56
    /// disk-headroom guard's input. `None` where the platform can't
    /// answer (the guard then stays permissive rather than blocking
    /// every materialization).
    pub fn available_bytes(&self) -> Result<Option<u64>, StoreError> {
        #[cfg(unix)]
        {
            let st = rustix::fs::statvfs(&self.root)
                .map_err(|e| StoreError::io(&self.root, io::Error::from(e)))?;
            Ok(Some(st.f_bavail.saturating_mul(st.f_frsize)))
        }
        #[cfg(not(unix))]
        {
            Ok(None)
        }
    }

    /// Filesystem capacity under the store root: `(total, available)`
    /// bytes, `None` where statvfs has no answer (same posture as
    /// [`Store::available_bytes`]). The D72 watermark reads this.
    pub fn fs_usage(&self) -> Result<Option<(u64, u64)>, StoreError> {
        #[cfg(unix)]
        {
            let st = rustix::fs::statvfs(&self.root)
                .map_err(|e| StoreError::io(&self.root, io::Error::from(e)))?;
            Ok(Some((
                st.f_blocks.saturating_mul(st.f_frsize),
                st.f_bavail.saturating_mul(st.f_frsize),
            )))
        }
        #[cfg(not(unix))]
        {
            Ok(None)
        }
    }

    /// Walk one namespace's shard tree — the recovery-scan primitive
    /// (D15). Yields `(hash, size)` per blob file; foreign files come back
    /// as [`StoreError::Foreign`] items so callers can count them without
    /// aborting the scan. `.obao4` sidecars are expected store files and
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

    /// [`Store::list`] fanned out over worker threads — the fast-recovery
    /// walk (metadata only: hash from the file name, size from stat; no
    /// bytes read). Workers split at the first shard level (256 dirs);
    /// results stream through a bounded channel in no particular order.
    /// `workers` is clamped to 1..=64; tuning waits on the M1 NFS bench.
    pub fn list_parallel(
        &self,
        ns: Namespace,
        workers: usize,
    ) -> std::sync::mpsc::IntoIter<Result<(Blake3, u64), StoreError>> {
        let workers = workers.clamp(1, 64);
        let (tx, rx) = std::sync::mpsc::sync_channel(1024);
        let root = self.root.join(ns.dir());
        // One pass to enumerate first-level shard dirs; anything odd at
        // this level is reported through the channel like list() would.
        let mut shard_dirs: Vec<PathBuf> = Vec::new();
        match fs::read_dir(&root) {
            Ok(rd) => {
                for entry in rd {
                    match entry {
                        Ok(e) if e.file_type().map(|t| t.is_dir()).unwrap_or(false) => {
                            shard_dirs.push(e.path());
                        }
                        Ok(e) => {
                            let _ = tx.send(Err(StoreError::Foreign { path: e.path() }));
                        }
                        Err(e) => {
                            let _ = tx.send(Err(StoreError::io(&root, e)));
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(Err(StoreError::io(&root, e)));
                return rx.into_iter();
            }
        }
        let shards = std::sync::Arc::new(std::sync::Mutex::new(shard_dirs));
        for _ in 0..workers {
            let shards = std::sync::Arc::clone(&shards);
            let tx = tx.clone();
            std::thread::spawn(move || {
                loop {
                    let Some(dir) = shards.lock().expect("shard queue").pop() else {
                        return;
                    };
                    let mut iter = ListIter {
                        stack: match fs::read_dir(&dir) {
                            Ok(rd) => vec![rd],
                            Err(e) => {
                                let _ = tx.send(Err(StoreError::io(&dir, e)));
                                continue;
                            }
                        },
                        pending_error: None,
                    };
                    for item in &mut iter {
                        if tx.send(item).is_err() {
                            return; // consumer gone
                        }
                    }
                }
            });
        }
        drop(tx); // workers hold the remaining senders
        rx.into_iter()
    }

    /// [`Store::verify`] that also recomputes the full alias tuple in
    /// the same read — the scrub upgrade path: fast recovery indexes
    /// blobs without reading them, and scrub back-fills aliases +
    /// verification with this. `None` tuple unless the blob is Valid.
    pub fn verify_with_aliases(
        &self,
        ns: Namespace,
        hash: &Blake3,
    ) -> Result<(VerifyOutcome, Option<datboi_core::alias::AliasTuple>), StoreError> {
        let Some(mut file) = self.get(ns, hash)? else {
            return Ok((VerifyOutcome::Missing, None));
        };
        let path = self.blob_path(ns, hash);
        let mut hasher = datboi_core::alias::AliasHasher::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(StoreError::io(&path, e)),
            }
        }
        let aliases = hasher.finalize();
        if aliases.blake3 == *hash {
            Ok((VerifyOutcome::Valid, Some(aliases)))
        } else {
            Ok((
                VerifyOutcome::Corrupt {
                    actual: aliases.blake3,
                },
                None,
            ))
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

    /// Materialization primitive (D25/D49): stream exactly `len` claimed
    /// bytes from `reader`, computing the bao outboard in the same pass
    /// (the tree root IS the blake3 hash — one traversal verifies and
    /// builds the sidecar). On a root match the blob and its sidecar are
    /// both published; on any mismatch — wrong bytes, short stream, or
    /// trailing bytes past `len` — nothing is, and the temp is removed.
    ///
    /// Unlike [`Store::put`], the reader is ALWAYS fully consumed and
    /// verified even when the blob is already present: replay licensing
    /// requires the *stream* to be proven, not the destination.
    pub fn put_with_obao(
        &self,
        ns: Namespace,
        expected: Blake3,
        len: u64,
        mut reader: impl Read,
    ) -> Result<PutOutcome, StoreError> {
        let temp = self.new_temp_path();
        let mut file = File::create_new(&temp).map_err(|e| StoreError::io(&temp, e))?;
        let result = crate::obao::compute(
            TeeToFile {
                inner: &mut reader,
                file: &mut file,
                path: &temp,
            },
            len,
        );
        let (root, sidecar) = match result {
            Ok(pair) => pair,
            Err(e) => {
                drop(file);
                let _ = fs::remove_file(&temp);
                return Err(StoreError::Obao {
                    path: temp,
                    source: e,
                });
            }
        };
        // A stream longer than the claimed size must fail verification
        // even if the first `len` bytes hash correctly — a deterministic
        // replay produces the claim exactly.
        let mut probe = [0u8; 1];
        let trailing = loop {
            match reader.read(&mut probe) {
                Ok(n) => break n > 0,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    drop(file);
                    let _ = fs::remove_file(&temp);
                    return Err(StoreError::io(&temp, e));
                }
            }
        };
        if root != expected || trailing {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(StoreError::HashMismatch {
                expected,
                // Trailing bytes: report the root we computed; the claim
                // is false either way.
                actual: root,
            });
        }

        let final_path = self.blob_path(ns, &expected);
        let outcome = if final_path.exists() {
            drop(file);
            let _ = fs::remove_file(&temp);
            PutOutcome::AlreadyPresent
        } else {
            file.sync_all().map_err(|e| StoreError::io(&temp, e))?;
            drop(file);
            let parent = final_path.parent().expect("blob paths have parents");
            fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
            fs::rename(&temp, &final_path).map_err(|e| StoreError::io(&final_path, e))?;
            fsync_dir(parent)?;
            PutOutcome::Stored
        };
        self.put_obao(ns, &expected, &sidecar)?;
        Ok(outcome)
    }

    /// Publish a bao outboard sidecar next to its blob, same tmp → fsync →
    /// rename discipline. Idempotent: an existing sidecar wins (same tree
    /// ⇒ same bytes). Empty outboards (blob ≤ one chunk group) are never
    /// written — absence IS the sidecar for small blobs.
    pub fn put_obao(
        &self,
        ns: Namespace,
        hash: &Blake3,
        sidecar: &[u8],
    ) -> Result<PutOutcome, StoreError> {
        if sidecar.is_empty() {
            return Ok(PutOutcome::AlreadyPresent);
        }
        let final_path = self.obao_path(ns, hash);
        if final_path.exists() {
            return Ok(PutOutcome::AlreadyPresent);
        }
        let temp = self.new_temp_path();
        let mut file = File::create_new(&temp).map_err(|e| StoreError::io(&temp, e))?;
        if let Err(e) = file.write_all(sidecar) {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(StoreError::io(&temp, e));
        }
        file.sync_all().map_err(|e| StoreError::io(&temp, e))?;
        drop(file);
        let parent = final_path.parent().expect("sidecar paths have parents");
        fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
        fs::rename(&temp, &final_path).map_err(|e| StoreError::io(&final_path, e))?;
        fsync_dir(parent)?;
        Ok(PutOutcome::Stored)
    }

    /// Load a blob's outboard sidecar. `Ok(Some(vec![]))` means the blob
    /// is resident and small enough (≤ one chunk group) that its outboard
    /// is empty; `Ok(None)` means no sidecar has been computed. The
    /// sidecar file is consulted FIRST: outboards must survive eviction
    /// of the literal (D49 rule 1), so this works with no blob on disk.
    /// (For an evicted small blob the caller decides emptiness from the
    /// indexed size — the store can't know the length of absent bytes.)
    pub fn get_obao(&self, ns: Namespace, hash: &Blake3) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.obao_path(ns, hash);
        match fs::read(&path) {
            Ok(bytes) => return Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(StoreError::io(&path, e)),
        }
        match self.len(ns, hash)? {
            Some(len) if crate::obao::outboard_size(len) == 0 => Ok(Some(Vec::new())),
            _ => Ok(None),
        }
    }

    /// Compute-and-publish a blob's outboard from its stored bytes if it
    /// isn't already present (one streaming read). Returns whether a
    /// sidecar now exists (false only for absent blobs).
    ///
    /// This is the "blessing" primitive: eviction (D49 rule 1) and
    /// recipe-served range reads both require the outboard to exist.
    pub fn ensure_obao(&self, ns: Namespace, hash: &Blake3) -> Result<bool, StoreError> {
        if self.get_obao(ns, hash)?.is_some() {
            return Ok(true);
        }
        let Some(len) = self.len(ns, hash)? else {
            return Ok(false);
        };
        let file = self
            .get(ns, hash)?
            .expect("len() saw the blob; single-writer tree");
        let path = self.blob_path(ns, hash);
        let (root, sidecar) =
            crate::obao::compute(io::BufReader::new(file), len).map_err(|e| StoreError::Obao {
                path: path.clone(),
                source: e,
            })?;
        if root != *hash {
            return Err(StoreError::HashMismatch {
                expected: *hash,
                actual: root,
            });
        }
        self.put_obao(ns, hash, &sidecar)?;
        Ok(true)
    }

    /// Verified range read (D49 rule 2): validate the covering chunk
    /// groups against the blob's outboard, then return exactly
    /// `offset..offset+len` (clamped to the blob). Fails — never returns
    /// unverified bytes — if the outboard is missing or validation fails.
    pub fn read_range_verified(
        &self,
        ns: Namespace,
        hash: &Blake3,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, StoreError> {
        let path = self.blob_path(ns, hash);
        let Some(blob_len) = self.len(ns, hash)? else {
            return Err(StoreError::io(
                &path,
                io::Error::new(io::ErrorKind::NotFound, "blob absent"),
            ));
        };
        let sidecar = self
            .get_obao(ns, hash)?
            .ok_or_else(|| StoreError::MissingOutboard { path: path.clone() })?;
        let file = self.get(ns, hash)?.expect("len() saw the blob");
        let start = offset.min(blob_len);
        let end = offset.saturating_add(len).min(blob_len);
        crate::obao::read_range_verified(&file, blob_len, hash, &sidecar, start..end)
            .map_err(|e| StoreError::Obao { path, source: e })
    }

    /// Remove a blob's LITERAL BYTES, keeping its outboard sidecar
    /// forever (D49 rule 1: the tree is what keeps recipe-served range
    /// reads verifiable after the bytes are gone).
    ///
    /// THIS IS THE ONLY BYTE-DESTROYING OPERATION IN THE STORE. The
    /// caller owns the D25/D21/D27 safety rules — a replayed-local
    /// recipe route grounded in retained literals must exist. The store
    /// cannot check that; `datboi-exec`'s eviction path is the one
    /// legitimate caller.
    ///
    /// Returns `false` if the blob wasn't resident (idempotent).
    pub fn evict_literal(&self, ns: Namespace, hash: &Blake3) -> Result<bool, StoreError> {
        let path = self.blob_path(ns, hash);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(StoreError::io(&path, e)),
        }
        // Make the removal durable the same way publishes are.
        fsync_dir(path.parent().expect("blob paths have parents"))?;
        Ok(true)
    }

    /// Remove a blob COMPLETELY: bytes and obao sidecar (D73 orphan
    /// deletion). Unlike [`Store::evict_literal`] — which keeps the
    /// sidecar so recipe-served range reads stay verifiable (D49) —
    /// an orphan by definition has no recipe to serve it, so the
    /// sidecar is dead weight. Returns whether bytes existed.
    pub fn remove_blob(&self, ns: Namespace, hash: &Blake3) -> Result<bool, StoreError> {
        let obao = self.obao_path(ns, hash);
        match fs::remove_file(&obao) {
            Ok(()) | Err(_) => {} // sidecar may never have existed
        }
        self.evict_literal(ns, hash)
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

    fn obao_path(&self, ns: Namespace, hash: &Blake3) -> PathBuf {
        self.root.join(layout::outboard_path(ns, hash))
    }

    fn new_temp_path(&self) -> PathBuf {
        let n = self.temp_counter.fetch_add(1, Ordering::Relaxed);
        self.root.join("tmp").join(format!(
            "{:08x}-{:016x}-{n:x}.temp",
            std::process::id(),
            self.temp_token,
        ))
    }

    /// A fresh staging path in tmp/ for externally-produced bytes (the
    /// daemon's upload ingest). Same uniqueness discipline as put
    /// temps (pid + open-token + counter) and the same sweep
    /// ([`Self::cleanup_temp`] — which never recurses, so staged
    /// uploads MUST stay flat files). `hint` (the original leaf name,
    /// sanitized to `[A-Za-z0-9._-]`, ≤64 chars) keeps downstream path
    /// labels (rescan cache, route provenance) legible.
    pub fn staging_path(&self, hint: &str) -> PathBuf {
        let n = self.temp_counter.fetch_add(1, Ordering::Relaxed);
        let hint: String = hint
            .chars()
            .map(|c| match c {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
                _ => '_',
            })
            .take(64)
            .collect();
        self.root.join("tmp").join(format!(
            "{:08x}-{:016x}-{n:x}-{hint}.temp",
            std::process::id(),
            self.temp_token,
        ))
    }
}

/// Read-side tee: every byte pulled by the outboard builder also lands in
/// the temp file, so materialize + verify + sidecar is one pass.
struct TeeToFile<'a, R: Read> {
    inner: &'a mut R,
    file: &'a mut File,
    path: &'a Path,
}

impl<R: Read> Read for TeeToFile<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.file.write_all(&buf[..n]).map_err(|e| {
                io::Error::new(e.kind(), format!("tee to {}: {e}", self.path.display()))
            })?;
        }
        Ok(n)
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
        "obao4" if stem.parse::<Blake3>().is_ok() => FileKind::Sidecar,
        _ => FileKind::Foreign,
    }
}

pub(crate) fn fsync_dir(dir: &Path) -> Result<(), StoreError> {
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

    /// Staged uploads live in tmp/ under sanitized names and are swept
    /// exactly like put temps.
    #[test]
    fn staging_paths_are_flat_sanitized_and_swept() {
        let (_dir, store) = temp_store();
        let path = store.staging_path("../we ird/Namé (v1).zip");
        assert_eq!(
            path.parent().expect("parent").file_name(),
            Some(std::ffi::OsStr::new("tmp")),
            "flat file directly in tmp/: {path:?}"
        );
        let name = path.file_name().expect("name").to_str().expect("utf8");
        assert!(
            name.ends_with("-.._we_ird_Nam___v1_.zip.temp"),
            "sanitized hint survives recognizably: {name}"
        );
        // Distinct per call even for the same hint.
        assert_ne!(path, store.staging_path("../we ird/Namé (v1).zip"));
        fs::write(&path, b"staged").expect("write");
        let removed = store.cleanup_temp(Duration::ZERO).expect("sweep");
        assert_eq!(removed, 1);
        assert!(!path.exists(), "swept with the rest of tmp/");
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
        fs::write(shard_dir.join(format!("{}.obao4", expected[0].0)), b"tree").expect("obao");
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
    fn obao_sidecar_lifecycle() {
        let (_dir, store) = temp_store();
        // Small blob: ensure_obao succeeds without writing a sidecar file.
        let small = vec![0x11u8; 4096];
        let small_hash = Blake3::compute(&small);
        store
            .put(Namespace::Data, small_hash, small.as_slice())
            .expect("put");
        assert!(
            store
                .ensure_obao(Namespace::Data, &small_hash)
                .expect("ensure")
        );
        assert_eq!(
            store.get_obao(Namespace::Data, &small_hash).expect("get"),
            Some(Vec::new())
        );
        assert_eq!(tree_files(&store.root).len(), 1, "no sidecar file written");
        assert_eq!(
            store
                .read_range_verified(Namespace::Data, &small_hash, 100, 200)
                .expect("verified read"),
            &small[100..300]
        );

        // Large blob: sidecar written once, verified reads work, absent
        // sidecar is a loud error first.
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
        let big_hash = Blake3::compute(&big);
        store
            .put(Namespace::Data, big_hash, big.as_slice())
            .expect("put");
        let err = store
            .read_range_verified(Namespace::Data, &big_hash, 0, 16)
            .expect_err("no sidecar yet");
        assert!(matches!(err, StoreError::MissingOutboard { .. }));
        assert!(
            store
                .ensure_obao(Namespace::Data, &big_hash)
                .expect("ensure")
        );
        assert!(
            store
                .ensure_obao(Namespace::Data, &big_hash)
                .expect("idempotent")
        );
        assert_eq!(
            store
                .read_range_verified(Namespace::Data, &big_hash, 65_000, 70_000)
                .expect("verified read"),
            &big[65_000..135_000]
        );
        // Read past EOF clamps.
        assert_eq!(
            store
                .read_range_verified(Namespace::Data, &big_hash, 199_999, 50)
                .expect("clamped read"),
            &big[199_999..]
        );

        // Corrupt the blob: the verified read fails with Obao, and the
        // listing still treats the sidecar as an expected store file.
        let path = store.blob_path(Namespace::Data, &big_hash);
        let mut bytes = fs::read(&path).expect("read blob");
        bytes[100_000] ^= 0xff;
        fs::write(&path, &bytes).expect("corrupt");
        let err = store
            .read_range_verified(Namespace::Data, &big_hash, 99_000, 4096)
            .expect_err("corruption detected");
        assert!(matches!(err, StoreError::Obao { .. }));

        // ensure_obao on an absent blob reports false, not an error.
        assert!(
            !store
                .ensure_obao(Namespace::Data, &Blake3::compute(b"never stored"))
                .expect("absent ok")
        );
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
