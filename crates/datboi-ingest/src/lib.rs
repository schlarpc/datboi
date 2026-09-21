//! The M1 ingest pipeline (docs/roadmap.md, D35/D40): walk sources,
//! hash everything once, store literals, and mint claims — never member
//! copies.
//!
//! Per file: consult the rescan cache (O(changed), the RomVault lesson),
//! stream into the store via `put_new` (single pass computes the full
//! alias tuple, D2; source untouched — D40 `--copy`), then look *inside*:
//!
//! - **Zip containers** stay literal; each STORED/DEFLATE member is
//!   hashed by streaming out of the stored blob and claimed via a derive
//!   recipe (`assemble@1` slice for STORED, `deflate-decompress@1` with a
//!   window param for DEFLATE) — member bytes are never stored (D35).
//! - **Header skippers** (D9): files matching a detector also get the
//!   transformed variant's alias tuple and, for `operation="none"`
//!   decisions, both-direction recipes (variant = slice of the stored
//!   file; file = header blob + variant). Swap-operation recipes are
//!   deferred until `swap@1` params are frozen — the variant's identity
//!   and aliases are still recorded.
//!
//! Recipes minted here are marked `Verified`, not `ReplayedLocal`: the
//! output hashes were computed from real bytes in this pass (D4), but the
//! drop path additionally requires a replay on this host (D25).
//!
//! Crash discipline: the rescan-cache row is written *last*, so a crash
//! re-processes the file; every write here is a content-addressed upsert,
//! so re-processing is idempotent (at-least-once semantics).
//!
//! Shape (D120): a bounded pool of workers does the hashing — the whole
//! of the above except the index — and ONE writer applies what they
//! conclude, because SQLite takes a single writer under WAL. The writer
//! retires verdicts in walk order, so the report is deterministic
//! whatever the parallelism, and all of a file's rows are still written
//! together with its `source_file` row last.

pub mod analyzers;
pub mod archive;
pub mod gcm;
pub mod iso9660;
pub mod narc;
pub mod nds;
pub mod refine;
pub mod wii;
pub mod xdvdfs;
pub mod zip;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use datboi_core::alias::{AliasHasher, AliasTuple};
use datboi_core::assemble::{AssembleParams, Segment};
use datboi_core::hash::Blake3;
use datboi_core::params::{DeflateWindow, ExtractorParams};
use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe, World};
use datboi_formats::skipper::{Detector, Operation};
use datboi_index::{Db, Namespace as IndexNs, RecipeSource, Residency, SeekClass};
use datboi_runtime::extractor::ExtractorHost;
use datboi_runtime::pipe;
use datboi_runtime::stream::{FileRandom, RangeRead};
use datboi_store_fs::{Namespace as StoreNs, PutOutcome, Store};

use crate::zip::{Method, ZipError};

/// The stamped `ex-unrar` component (D5/D6/D54) the rar derive recipes
/// pin — nix-built and embedded at compile time via
/// `DATBOI_COMPONENTS_DIR` (D66), never a checked-in artifact.
pub const EX_UNRAR_WASM: &[u8] = include_bytes!(concat!(
    env!("DATBOI_COMPONENTS_DIR"),
    "/datboi_ex_unrar.wasm"
));

/// The stamped `ex-7z` component (D110), same lane and same embedding
/// rules; 7z derive recipes pin it.
pub const EX_7Z_WASM: &[u8] =
    include_bytes!(concat!(env!("DATBOI_COMPONENTS_DIR"), "/datboi_ex_7z.wasm"));

/// Streaming buffer size for member hashing.
const CHUNK: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("i/o at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] datboi_store_fs::StoreError),
    #[error(transparent)]
    Index(#[from] datboi_index::IndexError),
    #[error(transparent)]
    Zip(#[from] ZipError),
    #[error("recipe construction: {0}")]
    Recipe(String),
    /// A hashing worker died on this file (D120's pool). Per-path and
    /// non-fatal like every other ingest failure — never a writer left
    /// waiting on a result nobody will send.
    #[error("hashing worker: {0}")]
    Worker(String),
}

impl IngestError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// Skipper evaluation buffers whole files; above this size detectors
    /// are skipped (reported), never partially applied.
    pub skipper_cap: u64,
    /// Ignore the `source_file` scan cache and re-read every path, even
    /// one whose (path, mtime, size) is unchanged. The cache is keyed on
    /// the SOURCE, so anything that changes what a re-read would CONCLUDE
    /// — a detector set arriving, a new dat making a previously unknown
    /// blob identifiable — leaves it confidently wrong. Without this the
    /// only way out was deleting `source_file` rows by hand.
    pub rescan: bool,
    /// How many files hash at once (D120). `0` derives it from the
    /// machine. The wall clock of an ingest is the `AliasHasher` chain
    /// — crc32 + md5 + sha1 + sha256 + blake3 over every byte, and md5
    /// has no hardware path — so one core's worth of it left seven idle
    /// on the adoption that prompted this.
    pub parallelism: usize,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            skipper_cap: 256 * 1024 * 1024,
            rescan: false,
            parallelism: 0,
        }
    }
}

impl IngestConfig {
    /// The worker count this config asks for, never zero.
    fn workers(&self) -> usize {
        if self.parallelism > 0 {
            return self.parallelism;
        }
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    }

    /// The ceiling on dispatched-but-unretired BUFFERED bytes (D120).
    /// A memory bound, not a throughput knob: the one lane that buffers
    /// a whole file is skipper evaluation, bounded per file by
    /// `skipper_cap`, and N workers would otherwise multiply that by N.
    /// Streaming files are charged nothing, so this never throttles the
    /// case the pool exists for. Never below `skipper_cap`: one
    /// eligible file must always be admissible.
    fn inflight_cap(&self) -> u64 {
        const PER_WORKER: u64 = 64 * 1024 * 1024;
        (self.workers() as u64)
            .saturating_mul(PER_WORKER)
            .max(self.skipper_cap)
    }
}

#[derive(Debug, Default)]
pub struct IngestReport {
    pub files_scanned: usize,
    /// Rescan-cache hits: path+mtime+size unchanged, nothing re-read.
    pub files_unchanged: usize,
    pub files_stored: usize,
    pub files_already_present: usize,
    /// CHDs whose declared internal sha1 was recorded (v3 and up; v1/v2
    /// declare only an md5 and are held as plain literals).
    pub chd_declared: usize,
    pub members_claimed: usize,
    /// 7z/rar members extracted into the CAS as resident blobs.
    pub members_extracted: usize,
    pub detector_hits: usize,
    /// Files over `skipper_cap` that were not detector-evaluated.
    pub skipper_skipped_large: usize,
    /// Per-path failures; ingest continues past them.
    pub errors: Vec<(PathBuf, String)>,
    /// (container, member, reason) — members outside the M1 subset.
    pub member_skips: Vec<(PathBuf, String, String)>,
    /// Non-fatal oddities worth surfacing (deferred swap recipes, …).
    pub notes: Vec<String>,
    /// Blob row ids that became RESIDENT during this run (stored files,
    /// extracted members, header blobs) — the narrow slice a refinement
    /// scheduler fast-tracks (D71). Ids, not hashes: they feed straight
    /// back into the same database's sweep queue.
    pub fresh_blobs: Vec<i64>,
}

/// Which extractor component a container runs (D58 rar, D110 7z) —
/// both share the lazily-built wasm host and the whole batch pipeline.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExFormat {
    Rar,
    SevenZ,
}

impl ExFormat {
    fn wasm(self) -> &'static [u8] {
        match self {
            ExFormat::Rar => EX_UNRAR_WASM,
            ExFormat::SevenZ => EX_7Z_WASM,
        }
    }
}

/// One compiled component + whether its bytes have been published into
/// the store this sweep (recipes pin it by hash).
struct ExtractorSlot {
    component: datboi_runtime::extractor::ExtractorComponent,
    published: bool,
}

/// Lazily-built extractor state (D58/D110): the wasm host plus a slot
/// per format, each compiled on the first container of that format.
struct ExtractorRt {
    host: ExtractorHost,
    rar: Option<ExtractorSlot>,
    sevenz: Option<ExtractorSlot>,
}

impl ExtractorRt {
    fn slot(&self, fmt: ExFormat) -> &ExtractorSlot {
        match fmt {
            ExFormat::Rar => self.rar.as_ref(),
            ExFormat::SevenZ => self.sevenz.as_ref(),
        }
        .expect("ensure_extractor first")
    }
}

/// One file the walk decided is worth reading, handed to a worker.
struct Job {
    /// Walk position — the commit queue's sort key (D120).
    seq: u64,
    /// The path as the walk found it (what the report names).
    path: PathBuf,
    canonical: PathBuf,
    /// The `source_file` key: the canonical path, or the caller's
    /// source name ([`Ingester::ingest_file`]).
    key: String,
    mtime_ns: i64,
    size: u64,
    /// What this file may BUFFER, charged to the in-flight budget —
    /// zero for everything that only streams (D120 amendment).
    weight: u64,
}

/// A worker's answer, back on the writer.
struct Done {
    seq: u64,
    path: PathBuf,
    /// The dispatched weight, released from the in-flight budget when
    /// this position retires.
    weight: u64,
    work: Box<Result<FileWork, IngestError>>,
}

/// What the writer decided about a staged file before dispatching it.
enum Staged {
    /// Rescan-cache hit: path+mtime+size unchanged, nothing to read.
    Unchanged,
    Work(Job),
}

/// One walk position awaiting its turn in the commit queue. Walk order
/// is report order (D120), so notes and failures queue up beside file
/// verdicts rather than jumping ahead of them.
enum Retire {
    Note(String),
    Failed(PathBuf, String),
    Unchanged,
    Work {
        path: PathBuf,
        weight: u64,
        work: Box<Result<FileWork, IngestError>>,
    },
}

pub struct Ingester<'a> {
    store: &'a Store,
    db: &'a mut Db,
    detectors: &'a [Detector],
    config: IngestConfig,
    /// Built on the first rar/7z container encountered (avoids the wasm
    /// engine cost when a sweep has neither). Writer-side state: D120
    /// keeps component extraction off the workers, since it mutates
    /// this lazily-built host and mints a recipe per member — and it
    /// already fans out internally (D89 batch pipes).
    extractor: Option<ExtractorRt>,
    /// Resident-blob ids accumulated across `record_resident_blob`
    /// calls; drained into `IngestReport::fresh_blobs` per run.
    fresh: Vec<i64>,
}

impl<'a> Ingester<'a> {
    pub fn new(store: &'a Store, db: &'a mut Db, detectors: &'a [Detector]) -> Self {
        Self {
            store,
            db,
            detectors,
            config: IngestConfig::default(),
            extractor: None,
            fresh: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_config(mut self, config: IngestConfig) -> Self {
        self.config = config;
        self
    }

    /// Ingest files and directory trees. Directories walk in sorted
    /// order and the report is written in that order — D120 fans the
    /// hashing out over a worker pool and orders the COMMIT QUEUE, not
    /// the work, so the same corpus produces the same report whatever
    /// the parallelism. Symlinks are skipped. Source identity is each
    /// file's canonical path — the walk never sees a source name
    /// ([`Ingester::ingest_file`] is the named-identity door).
    pub fn ingest(&mut self, paths: &[impl AsRef<Path>]) -> IngestReport {
        let mut report = IngestReport::default();
        let roots: Vec<PathBuf> = paths.iter().map(|p| p.as_ref().to_owned()).collect();
        // Copied out of `self` so the writer keeps its `&mut self`
        // while the pool reads these: both are `&'a`, neither borrows
        // the Ingester, and the store is `Sync` by construction (the
        // D89 extract path has published from threads since rar).
        let store = self.store;
        let detectors = self.detectors;
        let config = self.config.clone();
        let workers = config.workers();

        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let job_rx = Mutex::new(job_rx);
        let (done_tx, done_rx) = mpsc::channel::<Done>();

        std::thread::scope(|scope| {
            for _ in 0..workers {
                let done_tx = done_tx.clone();
                let job_rx = &job_rx;
                let config = &config;
                scope.spawn(move || {
                    loop {
                        // Held only across the recv: one lock per file
                        // is nothing beside a hash chain.
                        let job = {
                            let rx = job_rx.lock().unwrap_or_else(PoisonError::into_inner);
                            rx.recv()
                        };
                        let Ok(job) = job else { return };
                        // A panicking worker must become a per-path
                        // error, never a writer blocked forever on a
                        // result nobody will send.
                        let work = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            hash_file(store, detectors, config, &job)
                        }))
                        .unwrap_or_else(|_| {
                            Err(IngestError::Worker(format!(
                                "hashing {} panicked",
                                job.path.display()
                            )))
                        });
                        let done = Done {
                            seq: job.seq,
                            path: job.path,
                            weight: job.weight,
                            work: Box::new(work),
                        };
                        if done_tx.send(done).is_err() {
                            return;
                        }
                    }
                });
            }
            // Ours would otherwise keep the channel alive forever, and
            // a dead pool has to read as a disconnect.
            drop(done_tx);
            self.write_loop(Walk::new(roots), &job_tx, &done_rx, &mut report);
            // Closing the job channel is what retires the pool; the
            // scope then joins it.
            drop(job_tx);
        });

        report.fresh_blobs = std::mem::take(&mut self.fresh);
        report
    }

    /// Ingest ONE file under a caller-supplied source identity (staged
    /// web uploads): the `source_file` key becomes `source_name`
    /// instead of the throwaway staging path, so provenance reads
    /// "roms/pack.zip", re-uploads update one row instead of minting
    /// dead staging-path rows, and the mtime+size check still defeats
    /// false rescan-cache hits. A distinct entry point BY CONSTRUCTION:
    /// a directory walk under one name would collide keys, so anything
    /// but a regular file is refused here and the walk path has no
    /// source name to misuse. One file needs no pool — it runs the same
    /// hash/apply pair the workers and the writer run.
    pub fn ingest_file(&mut self, path: &Path, source_name: &str) -> IngestReport {
        let mut report = IngestReport::default();
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() => {
                report.files_scanned += 1;
                match self.stage(0, path, &meta, Some(source_name)) {
                    Ok(Staged::Unchanged) => report.files_unchanged += 1,
                    Ok(Staged::Work(job)) => {
                        let work = hash_file(self.store, self.detectors, &self.config, &job);
                        self.retire_work(job.path, work, &mut report);
                    }
                    Err(e) => report.errors.push((path.to_owned(), e.to_string())),
                }
            }
            Ok(_) => report.errors.push((
                path.to_owned(),
                "a named source must be a regular file".to_owned(),
            )),
            Err(e) => report.errors.push((path.to_owned(), e.to_string())),
        }
        report.fresh_blobs = std::mem::take(&mut self.fresh);
        report
    }

    /// The single writer (D120): walk, cache-check, dispatch within the
    /// in-flight budget, retire verdicts in walk order. EVERY `Db`
    /// mutation an ingest makes happens on this thread, which is what
    /// keeps the report deterministic and the crash discipline honest
    /// (a crash truncates the run at a walk-order prefix).
    fn write_loop(
        &mut self,
        mut walk: Walk,
        jobs: &mpsc::Sender<Job>,
        done: &mpsc::Receiver<Done>,
        report: &mut IngestReport,
    ) {
        // Ceiling on unretired walk positions. The reorder buffer holds
        // verdicts, not file bytes, so this is an entry count — it only
        // bites when one slow file holds the head of the queue, and it
        // is what bounds a queue of files the byte budget charges
        // nothing for.
        const REORDER_CAP: usize = 256;

        let cap = self.config.inflight_cap();
        let mut next_seq = 0u64;
        let mut retire_seq = 0u64;
        let mut pending: BTreeMap<u64, Retire> = BTreeMap::new();
        let mut inflight_bytes = 0u64;
        // Dispatched, result not yet received: the writer may block on
        // the done channel exactly when this is non-zero.
        let mut outstanding = 0usize;
        let mut walking = true;
        // A position the budget turned away: the walk stops behind it
        // rather than dispatching past it, so order is never lost.
        let mut held: Option<Job> = None;

        loop {
            // Weight is what a file may BUFFER (D120 amendment), so a
            // streaming file is always admissible and only the skipper
            // lane ever queues here. Dispatch is in walk order, so when
            // the head of the queue is the one asking, nothing below it
            // is unretired and the budget is empty — the head always
            // runs and cannot deadlock against the reorder buffer that
            // is waiting on it.
            if let Some(job) = held.take() {
                if admits(inflight_bytes, cap, job.weight) {
                    dispatch(
                        jobs,
                        job,
                        &mut inflight_bytes,
                        &mut outstanding,
                        &mut pending,
                    );
                } else {
                    held = Some(job);
                }
            }

            while held.is_none() && walking && pending.len() + outstanding < REORDER_CAP {
                let Some(step) = walk.next() else {
                    walking = false;
                    break;
                };
                let seq = next_seq;
                next_seq += 1;
                let (path, meta) = match step {
                    Step::Note(note) => {
                        pending.insert(seq, Retire::Note(note));
                        continue;
                    }
                    Step::Failed(path, err) => {
                        pending.insert(seq, Retire::Failed(path, err));
                        continue;
                    }
                    Step::File(path, meta) => (path, meta),
                };
                report.files_scanned += 1;
                match self.stage(seq, &path, &meta, None) {
                    Ok(Staged::Unchanged) => {
                        pending.insert(seq, Retire::Unchanged);
                    }
                    Ok(Staged::Work(job)) => {
                        if admits(inflight_bytes, cap, job.weight) {
                            dispatch(
                                jobs,
                                job,
                                &mut inflight_bytes,
                                &mut outstanding,
                                &mut pending,
                            );
                        } else {
                            held = Some(job);
                        }
                    }
                    Err(e) => {
                        pending.insert(seq, Retire::Failed(path, e.to_string()));
                    }
                }
            }

            // Retire strictly in walk order — the report is written
            // here and nowhere else.
            while let Some(item) = pending.remove(&retire_seq) {
                retire_seq += 1;
                match item {
                    Retire::Note(note) => report.notes.push(note),
                    Retire::Failed(path, err) => report.errors.push((path, err)),
                    Retire::Unchanged => report.files_unchanged += 1,
                    Retire::Work { path, weight, work } => {
                        inflight_bytes = inflight_bytes.saturating_sub(weight);
                        self.retire_work(path, *work, report);
                    }
                }
            }

            if outstanding == 0 {
                // Nothing is owed: either the walk is done (and the
                // retire pass above drained everything, since every
                // unretired position is resolved) or there is more to
                // dispatch.
                if !walking && pending.is_empty() && held.is_none() {
                    return;
                }
                continue;
            }
            match done.recv() {
                Ok(d) => {
                    outstanding -= 1;
                    pending.insert(
                        d.seq,
                        Retire::Work {
                            path: d.path,
                            weight: d.weight,
                            work: d.work,
                        },
                    );
                }
                Err(_) => {
                    // Every worker is gone with results owed; there is
                    // nothing left to wait for.
                    report.errors.push((
                        PathBuf::new(),
                        format!("hashing pool died with {outstanding} file(s) in flight"),
                    ));
                    return;
                }
            }
        }
    }

    /// Canonicalize, key and cache-check one file — on the writer,
    /// because `lookup_unchanged_source` is a `Db` read and because a
    /// cache hit must never cost a worker a read (D120: not reading the
    /// file is the entire point of the cache).
    ///
    /// `source_name` is [`Ingester::ingest_file`]'s named identity; the
    /// walk always passes `None` (canonical-path identity).
    fn stage(
        &mut self,
        seq: u64,
        path: &Path,
        meta: &fs::Metadata,
        source_name: Option<&str>,
    ) -> Result<Staged, IngestError> {
        let canonical = fs::canonicalize(path).map_err(|e| IngestError::io(path, e))?;
        let key =
            source_name.map_or_else(|| canonical.to_string_lossy().into_owned(), str::to_owned);
        let mtime_ns = mtime_ns(meta);
        let size = meta.len();

        if !self.config.rescan
            && self
                .db
                .lookup_unchanged_source(&key, mtime_ns, size)?
                .is_some()
        {
            return Ok(Staged::Unchanged);
        }
        // What this file may buffer whole: skipper evaluation, and only
        // skipper evaluation (D120 amendment). Charging a container
        // that merely LOOKS eligible over-charges, which is the safe
        // direction; charging a 40 GB CHD that streams through 64 KiB
        // would serialize exactly the files the pool exists for.
        let weight = if self.detectors.is_empty() || size > self.config.skipper_cap {
            0
        } else {
            size
        };
        Ok(Staged::Work(Job {
            seq,
            path: path.to_owned(),
            canonical,
            key,
            mtime_ns,
            size,
            weight,
        }))
    }

    fn retire_work(
        &mut self,
        path: PathBuf,
        work: Result<FileWork, IngestError>,
        report: &mut IngestReport,
    ) {
        let outcome = match work {
            Ok(work) => self.apply(&path, work, report),
            Err(e) => Err(e),
        };
        if let Err(e) = outcome {
            report.errors.push((path, e.to_string()));
        }
    }

    /// Record one file's verdict: the same rows the serial pipeline
    /// wrote, in the same order, and the `source_file` row still LAST
    /// so a crash re-processes the file (module doc's crash
    /// discipline). Every write below is a content-addressed upsert.
    fn apply(
        &mut self,
        path: &Path,
        work: FileWork,
        report: &mut IngestReport,
    ) -> Result<(), IngestError> {
        match work.stored {
            PutOutcome::Stored => report.files_stored += 1,
            PutOutcome::AlreadyPresent => report.files_already_present += 1,
        }
        let blob_id = self.record_resident_blob(&work.hash, &work.aliases)?;
        report.notes.extend(work.notes);
        for (member, reason) in work.member_skips {
            report.member_skips.push((path.to_owned(), member, reason));
        }
        // Failures from looking INSIDE are per-path and non-fatal: a
        // container we could not read is still a literal we hold, and
        // it still earns its rescan-cache row.
        for err in work.errors {
            report.errors.push((path.to_owned(), err));
        }
        match work.inside {
            Inside::Opaque => {}
            Inside::ChdDeclared(sha1) => {
                self.db.insert_declared_chd_sha1(blob_id, &sha1)?;
                report.chd_declared += 1;
            }
            Inside::Zip(members) => self.claim_zip_members(members, report)?,
            Inside::Component(fmt) => {
                let extracted = match fmt {
                    ExFormat::SevenZ => self.process_7z(&work.hash, report),
                    ExFormat::Rar => self.process_rar(&work.hash, report),
                };
                if let Err(e) = extracted {
                    report.errors.push((path.to_owned(), e));
                }
            }
            Inside::Detector(claim) => self.claim_detector(*claim, report)?,
            Inside::SkipperTooLarge => report.skipper_skipped_large += 1,
        }

        // Last, so a crash before this point re-processes the file.
        self.db.upsert_source_file(
            &work.key,
            work.mtime_ns,
            work.size,
            Some(blob_id),
            now_unix(),
        )?;
        Ok(())
    }

    /// Claim every member a worker hashed out of a zip container.
    fn claim_zip_members(
        &mut self,
        members: Vec<MemberClaim>,
        report: &mut IngestReport,
    ) -> Result<(), IngestError> {
        for member in members {
            self.record_absent_blob(&member.tuple)?;
            match member.recipe {
                // The empty member: the worker stored the empty literal
                // so the identity is grounded, and assemble@1 rejects
                // empty segment lists by design — no recipe to mint.
                None => {
                    self.db.upsert_blob(
                        &member.tuple.blake3,
                        Some(0),
                        IndexNs::Data,
                        Residency::Resident,
                    )?;
                }
                Some((recipe, seek)) => self.record_recipe(&recipe, seek)?,
            }
            report.members_claimed += 1;
        }
        Ok(())
    }

    /// Record a detector hit's dual identity (D9) — the variant, and
    /// the both-direction recipes when the decision licensed them.
    fn claim_detector(
        &mut self,
        claim: DetectorClaim,
        report: &mut IngestReport,
    ) -> Result<(), IngestError> {
        report.detector_hits += 1;
        self.record_absent_blob(&claim.variant)?;
        // A swap-operation decision aliases the variant and stops; the
        // deferral note rides the file's notes.
        let Some((derive, seek)) = claim.derive else {
            return Ok(());
        };
        self.record_recipe(&derive, seek)?;
        if let Some((header, rebuild, rebuild_seek)) = claim.rebuild {
            self.record_resident_blob(&header.blake3, &header)?;
            self.record_recipe(&rebuild, rebuild_seek)?;
        }
        Ok(())
    }

    /// Extract every 7z member into the CAS through the `ex-7z`
    /// component (D110): 7-Zip's own C decoder inside the wasm sandbox,
    /// streaming — each member lands resident AND carries a derive
    /// recipe, exactly the rar shape. Coder coverage equals upstream
    /// 7zDec's (LZMA/LZMA2/Copy/PPMd mains, Delta + branch filters,
    /// BCJ2); folder graphs beyond that refuse whole and the container
    /// stays an opaque literal (D24) — the same posture as a rar the
    /// extractor cannot open.
    fn process_7z(
        &mut self,
        container_hash: &Blake3,
        report: &mut IngestReport,
    ) -> Result<(), String> {
        self.extract_via_component(ExFormat::SevenZ, container_hash, report)
    }

    /// Extract every rar member into the CAS through the `ex-unrar`
    /// component (D58): unrar's C++ runs inside the wasm sandbox, so
    /// extraction is deterministic-by-construction. Each member lands
    /// resident AND carries a DERIVE RECIPE (container→member through the
    /// component) so it can be evicted and rebuilt — the recipe re-runs the
    /// same component, never a recompressor (rar rebuild stays infeasible).
    fn process_rar(
        &mut self,
        container_hash: &Blake3,
        report: &mut IngestReport,
    ) -> Result<(), String> {
        self.extract_via_component(ExFormat::Rar, container_hash, report)
    }

    /// The shared component-extraction path (D58 rar, D110 7z): decode
    /// members in BATCHES (D89) — one guest pass serves the whole batch,
    /// so each solid block decodes once (the single-member ABI made this
    /// sweep O(n²) in solid archives). Each member streams into the CAS
    /// through its own bounded pipe with hashing on the consumer threads,
    /// so decode overlaps hash+store; neither the container nor any
    /// member is ever whole in memory. Each non-empty member gains a
    /// container→member derive recipe pinning the format's component.
    fn extract_via_component(
        &mut self,
        fmt: ExFormat,
        container_hash: &Blake3,
        report: &mut IngestReport,
    ) -> Result<(), String> {
        self.ensure_extractor(fmt)?;
        let container_len = self
            .store
            .len(StoreNs::Data, container_hash)
            .map_err(|e| e.to_string())?
            .unwrap_or(0);
        let members = self.extractor_enumerate(fmt, container_hash, container_len)?;

        // Fuel scales with the WHOLE archive per batch (container +
        // every member), not the batch's slice: a solid folder decodes
        // predecessors regardless of the request set, so the guest's
        // instruction count follows total unpacked size. Same
        // calibration as the exec replay path (fuel exists to kill
        // runaways; generosity costs nothing — datboi_exec doc).
        let total_unpacked = members
            .iter()
            .map(|m| m.size)
            .fold(0u64, u64::saturating_add);
        let fuel = datboi_exec::fuel_for_bytes(container_len.saturating_add(total_unpacked));

        // The batch cap bounds consumer threads; solid decode restarts
        // once per batch, which is the accepted cost of the cap.
        const EXTRACT_BATCH: usize = 128;
        for chunk in members.chunks(EXTRACT_BATCH) {
            let stored =
                self.extractor_extract_batch_into_store(fmt, container_hash, chunk, fuel)?;
            for (member, (member_hash, aliases)) in chunk.iter().zip(stored) {
                if aliases.size != member.size {
                    // The mismatched bytes already landed in the CAS
                    // (streaming means we learn the size last); they are
                    // content-addressed and unreferenced — GC fodder, not
                    // corruption. Refuse the archive before minting any
                    // claim to them.
                    return Err(format!(
                        "member {:?}: extractor produced {} bytes, header claims {}",
                        member.name, aliases.size, member.size
                    ));
                }
                self.record_resident_blob(&member_hash, &aliases)
                    .map_err(|e| e.to_string())?;

                // Mint the container→member derive recipe (makes the
                // member evictable). Empty members need no recipe
                // (nothing to rebuild).
                if member.size > 0 {
                    let params = ExtractorParams {
                        member_ix: member.ix,
                    }
                    .encode();
                    let recipe = Recipe {
                        op: Op::Wasm {
                            component: Blake3::compute(fmt.wasm()),
                            world: World::Extractor1,
                            export: World::Extractor1
                                .required_export()
                                .expect("extractor world fixes its export")
                                .into(),
                        },
                        inputs: vec![InputRef {
                            hash: *container_hash,
                            role: None,
                        }],
                        outputs: vec![OutputRef {
                            hash: member_hash,
                            size: member.size,
                            name: Some(member.name.clone()),
                        }],
                        params,
                    };
                    mint_recipe(self.store, self.db, &recipe, SeekClass::Opaque)
                        .map_err(|e| e.to_string())?;
                }
                report.members_extracted += 1;
            }
        }
        Ok(())
    }

    /// Lazily build the extractor host + compile the format's pinned
    /// component, and publish the component into the store+index once per
    /// sweep (recipes pin it by hash, so a later replay can load it).
    fn ensure_extractor(&mut self, fmt: ExFormat) -> Result<(), String> {
        if self.extractor.is_none() {
            let host =
                ExtractorHost::new(datboi_runtime::Limits::default()).map_err(|e| e.to_string())?;
            self.extractor = Some(ExtractorRt {
                host,
                rar: None,
                sevenz: None,
            });
        }
        let rt = self.extractor.as_mut().expect("just set");
        let missing = match fmt {
            ExFormat::Rar => rt.rar.is_none(),
            ExFormat::SevenZ => rt.sevenz.is_none(),
        };
        if missing {
            let component = rt.host.load(fmt.wasm()).map_err(|e| e.to_string())?;
            let slot = ExtractorSlot {
                component,
                published: false,
            };
            match fmt {
                ExFormat::Rar => rt.rar = Some(slot),
                ExFormat::SevenZ => rt.sevenz = Some(slot),
            }
        }
        let published = match fmt {
            ExFormat::Rar => rt.rar.as_ref().expect("just set").published,
            ExFormat::SevenZ => rt.sevenz.as_ref().expect("just set").published,
        };
        if !published {
            let wasm = fmt.wasm();
            let hash = Blake3::compute(wasm);
            self.store
                .put(StoreNs::Data, hash, wasm)
                .map_err(|e| e.to_string())?;
            self.db
                .upsert_blob(
                    &hash,
                    Some(wasm.len() as u64),
                    IndexNs::Data,
                    Residency::Resident,
                )
                .map_err(|e| e.to_string())?;
            let rt = self.extractor.as_mut().expect("just set");
            match fmt {
                ExFormat::Rar => rt.rar.as_mut().expect("just set").published = true,
                ExFormat::SevenZ => rt.sevenz.as_mut().expect("just set").published = true,
            }
        }
        Ok(())
    }

    /// The stored container as a seekable resource for the component —
    /// a fresh handle per call (the extractor owns the cursor).
    fn container_random(&self, hash: &Blake3) -> Result<Box<dyn RangeRead>, String> {
        let file = self
            .store
            .get(StoreNs::Data, hash)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "rar container vanished from the store".to_owned())?;
        Ok(Box::new(FileRandom::new(file).map_err(|e| e.to_string())?))
    }

    fn extractor_enumerate(
        &self,
        fmt: ExFormat,
        container: &Blake3,
        container_len: u64,
    ) -> Result<Vec<datboi_runtime::extractor::Member>, String> {
        let rt = self.extractor.as_ref().expect("ensure_extractor first");
        rt.host
            .enumerate_fueled(
                &rt.slot(fmt).component,
                vec![self.container_random(container)?],
                &[],
                // The header walk's work is bounded by the container
                // itself (compressed headers decode whole).
                Some(datboi_exec::fuel_for_bytes(container_len)),
            )
            .map_err(|e| e.to_string())
    }

    /// Decode a batch of members into the CAS in ONE guest pass (D89):
    /// the extractor pushes each member into its own bounded pipe while
    /// a consumer thread per pipe hashes and stores the pull side
    /// (`put_new`). Members arrive in archive order, so at any moment
    /// one pipe is filling and the rest of the consumers are blocked —
    /// threads are cheap, the pipes bound memory. An extractor failure
    /// surfaces to every reader as an error (never a clean EOF), so
    /// `put_new` deletes its temps and publishes nothing.
    fn extractor_extract_batch_into_store(
        &self,
        fmt: ExFormat,
        container: &Blake3,
        members: &[datboi_runtime::extractor::Member],
        fuel: u64,
    ) -> Result<Vec<(Blake3, AliasTuple)>, String> {
        let rt = self.extractor.as_ref().expect("ensure_extractor first");
        // Only the store crosses into the consumer threads — the rest of
        // self (the sqlite handle) is not Sync.
        let store = self.store;
        let archive = self.container_random(container)?;
        let mut requests: Vec<(u32, Box<dyn std::io::Write + Send>)> = Vec::new();
        let mut consumers_in = Vec::new();
        for member in members {
            let (w, r, h) = pipe::pipe();
            requests.push((member.ix, Box::new(w)));
            consumers_in.push((member, r, h));
        }
        std::thread::scope(|s| {
            let guest = s.spawn(move || {
                rt.host.extract_fueled(
                    &rt.slot(fmt).component,
                    vec![archive],
                    &[],
                    requests,
                    Some(fuel),
                )
            });
            let consumers: Vec<_> = consumers_in
                .into_iter()
                .map(|(member, r, h)| {
                    let handle = s.spawn(move || {
                        let (hash, aliases, _) = store
                            .put_new(StoreNs::Data, r)
                            .map_err(|e| format!("storing member {:?}: {e}", member.name))?;
                        Ok::<_, String>((hash, aliases))
                    });
                    (handle, h)
                })
                .collect();
            let guest_result = guest.join().expect("guest thread never panics");
            // Verdict first, then finish: consumers blocked at
            // channel-disconnect wait for it (the exec pipe-race fix).
            for (_, h) in &consumers {
                if let Err(e) = &guest_result {
                    h.fail(format!("extractor failed: {e}"));
                }
                h.finish();
            }
            let mut stored = Vec::with_capacity(consumers.len());
            for (handle, _) in consumers {
                stored.push(handle.join().expect("consumer thread never panics"));
            }
            // The guest's own error explains a consumer failure better
            // than the downstream pipe error does.
            if let Err(e) = guest_result {
                return Err(e.to_string());
            }
            stored.into_iter().collect()
        })
    }

    fn record_resident_blob(
        &mut self,
        hash: &Blake3,
        aliases: &AliasTuple,
    ) -> Result<i64, IngestError> {
        let id =
            self.db
                .upsert_blob(hash, Some(aliases.size), IndexNs::Data, Residency::Resident)?;
        self.db.insert_aliases(id, aliases)?;
        self.db.set_verified(id, now_unix())?;
        self.fresh.push(id);
        Ok(id)
    }

    /// A claimed identity whose literal is not stored (members, variants).
    fn record_absent_blob(&mut self, aliases: &AliasTuple) -> Result<i64, IngestError> {
        let id = self.db.upsert_blob(
            &aliases.blake3,
            Some(aliases.size),
            IndexNs::Data,
            Residency::Absent,
        )?;
        self.db.insert_aliases(id, aliases)?;
        Ok(id)
    }

    /// Publish a recipe object (meta namespace) and index it as Verified —
    /// idempotent across re-ingest (the recipe row is keyed by its blob).
    fn record_recipe(&mut self, recipe: &Recipe, seek: SeekClass) -> Result<(), IngestError> {
        mint_recipe(self.store, self.db, recipe, seek)?;
        Ok(())
    }
}

/// Whether the in-flight budget has room for `weight`. A streaming
/// file (weight zero) always passes, and a file heavier than the whole
/// cap runs alone rather than never.
const fn admits(inflight: u64, cap: u64, weight: u64) -> bool {
    weight == 0 || inflight == 0 || inflight.saturating_add(weight) <= cap
}

/// Hand one job to the pool, charging its weight — or, if the pool is
/// already gone, park the failure in the commit queue where its walk
/// position is.
fn dispatch(
    jobs: &mpsc::Sender<Job>,
    job: Job,
    inflight_bytes: &mut u64,
    outstanding: &mut usize,
    pending: &mut BTreeMap<u64, Retire>,
) {
    let (seq, weight) = (job.seq, job.weight);
    *inflight_bytes = inflight_bytes.saturating_add(weight);
    *outstanding += 1;
    if let Err(mpsc::SendError(job)) = jobs.send(job) {
        *inflight_bytes = inflight_bytes.saturating_sub(weight);
        *outstanding -= 1;
        pending.insert(
            seq,
            Retire::Failed(job.path, "hashing pool stopped".to_owned()),
        );
    }
}

/// Everything one file's hashing concluded — the value a worker hands
/// the writer (D120). The worker has already made every STORE write it
/// needs (content-addressed, idempotent, safe from any thread); what
/// travels here is only what the index has to learn.
struct FileWork {
    key: String,
    mtime_ns: i64,
    size: u64,
    hash: Blake3,
    aliases: AliasTuple,
    stored: PutOutcome,
    inside: Inside,
    /// Per-path failures raised while looking inside.
    errors: Vec<String>,
    /// (member, reason) — the container's path is the writer's.
    member_skips: Vec<(String, String)>,
    notes: Vec<String>,
}

/// What the head sniff found, and what the writer owes the index for it.
enum Inside {
    /// Nothing to look into (or a CHD that declares no sha1 at all —
    /// the note carries that).
    Opaque,
    /// A CHD's declared internal sha1 (v3, v4 or v5): the identity MAME
    /// disk claims reference. Header-only, so audit grades it
    /// `probable` (D44) until the `chd-verify` sweep decompresses it.
    ChdDeclared([u8; 20]),
    Zip(Vec<MemberClaim>),
    /// 7z/rar: extraction runs on the writer (D120).
    Component(ExFormat),
    Detector(Box<DetectorClaim>),
    /// Over `skipper_cap`: detectors are skipped, never half-applied.
    SkipperTooLarge,
}

/// One zip member's claim: its identity, and the derive recipe that
/// rebuilds it from the container (`None` for the empty member).
struct MemberClaim {
    tuple: AliasTuple,
    recipe: Option<(Recipe, SeekClass)>,
}

/// A detector hit's dual identity (D9).
struct DetectorClaim {
    variant: AliasTuple,
    /// `None` for a swap-operation decision: the variant is aliased
    /// only, until `swap@1` params freeze.
    derive: Option<(Recipe, SeekClass)>,
    /// The common prefix-header shape: the header blob's identity plus
    /// the file = header + variant rebuild.
    rebuild: Option<(AliasTuple, Recipe, SeekClass)>,
}

/// Hash one file and everything inside it. Pure CPU and source I/O —
/// no `Db`, which is what lets N of these run at once (D120).
fn hash_file(
    store: &Store,
    detectors: &[Detector],
    config: &IngestConfig,
    job: &Job,
) -> Result<FileWork, IngestError> {
    let source = File::open(&job.canonical).map_err(|e| IngestError::io(&job.canonical, e))?;
    let (hash, aliases, stored) = store.put_new(StoreNs::Data, source)?;
    let mut work = FileWork {
        key: job.key.clone(),
        mtime_ns: job.mtime_ns,
        size: job.size,
        hash,
        aliases,
        stored,
        inside: Inside::Opaque,
        errors: Vec::new(),
        member_skips: Vec::new(),
        notes: Vec::new(),
    };

    // Look inside the *stored* bytes (verifies what we published).
    let mut blob = store.get(StoreNs::Data, &hash)?.expect("just published");
    // One head read serves both container sniffs (zip magic is 4 bytes,
    // a CHD v5 header is 124).
    let mut head = [0u8; datboi_formats::chd::CHD_V5_HEADER_LEN];
    let head_len = read_head(&mut blob, &mut head).map_err(|e| IngestError::io(&job.path, e))?;
    if let Some(chd) = datboi_formats::chd::try_parse_header(&head[..head_len]) {
        work.inside = read_chd(&job.path, chd, &mut work.notes);
    } else if zip::looks_like_zip(&head[..head_len]) {
        match hash_zip_members(store, &hash, &mut blob, &mut work.member_skips) {
            Ok(members) => work.inside = Inside::Zip(members),
            Err(e) => work.errors.push(e.to_string()),
        }
    } else if archive::looks_like_7z(&head[..head_len]) {
        work.inside = Inside::Component(ExFormat::SevenZ);
    } else if archive::looks_like_rar(&head[..head_len]) {
        work.inside = Inside::Component(ExFormat::Rar);
    } else if !detectors.is_empty() {
        if job.size <= config.skipper_cap {
            blob.seek(SeekFrom::Start(0))
                .map_err(|e| IngestError::io(&job.path, e))?;
            let mut bytes = Vec::with_capacity(job.size as usize);
            blob.read_to_end(&mut bytes)
                .map_err(|e| IngestError::io(&job.path, e))?;
            work.inside = evaluate_detectors(store, detectors, &bytes, &hash, &mut work.notes)?;
        } else {
            work.inside = Inside::SkipperTooLarge;
        }
    }
    Ok(work)
}

/// Record the header's declared internal sha1 — the identity MAME disk
/// claims reference. Header-only, so the declaration grades as
/// `probable` in audit (D44); the `chd-verify` sweep is what upgrades
/// it by actually decompressing the file.
///
/// v1 and v2 predate sha1 in the format, so they are parsed, reported
/// and then held as ordinary opaque literals: there is no digest they
/// could answer a modern disk claim with, and offering their md5 in a
/// sha1's place would be a lie of exactly the kind D44 forbids.
fn read_chd(
    path: &Path,
    chd: Result<datboi_formats::chd::ChdHeader, datboi_formats::chd::ChdError>,
    notes: &mut Vec<String>,
) -> Inside {
    let header = match chd {
        Ok(h) => h,
        Err(e) => {
            notes.push(format!("{}: {e}; stored as opaque bytes", path.display()));
            return Inside::Opaque;
        }
    };
    if header.has_parent() {
        notes.push(format!(
            "{}: delta CHD (has a parent); recorded, but standalone rebuild is impossible",
            path.display()
        ));
    }
    match header.declared_disk_sha1() {
        Some(sha1) => Inside::ChdDeclared(sha1),
        None => {
            notes.push(format!(
                "{}: CHD v{} declares an md5 and no sha1, so no MAME disk claim can name it; \
                 stored as opaque bytes",
                path.display(),
                header.version
            ));
            Inside::Opaque
        }
    }
}

/// Hash every supported member of a stored zip container and build its
/// claim (D35: member bytes are never stored — a recipe rebuilds them
/// from the container, which stays a literal).
fn hash_zip_members(
    store: &Store,
    zip_hash: &Blake3,
    blob: &mut datboi_store_fs::Blob,
    skips: &mut Vec<(String, String)>,
) -> Result<Vec<MemberClaim>, IngestError> {
    let parsed = zip::parse_members(blob)?;
    for skip in parsed.skipped {
        skips.push((skip.name, skip.reason.to_owned()));
    }
    let mut claims = Vec::with_capacity(parsed.members.len());
    for member in parsed.members {
        let (tuple, sidecar) = match hash_member(blob, &member) {
            Ok(t) => t,
            Err(reason) => {
                skips.push((member.name, reason));
                continue;
            }
        };
        // D63 amendment: the member blob itself is never stored (D35),
        // but its outboard is — a sidecar with no `.data` beside it is
        // exactly the shape D49 rule 1 already keeps after an eviction,
        // and the store scan skips `.obao4` files either way. Without
        // it a `deflate-decompress@1` range read has no carve-out and
        // no tree, so it cannot be served at all.
        if let Some(sidecar) = sidecar {
            store.put_obao(StoreNs::Data, &tuple.blake3, &sidecar)?;
        }

        if member.uncomp_size == 0 {
            // The empty output needs no recipe (assemble@1 rejects
            // empty segment lists by design); store the empty literal
            // so the identity is grounded.
            store.put(StoreNs::Data, tuple.blake3, std::io::empty())?;
            claims.push(MemberClaim {
                tuple,
                recipe: None,
            });
            continue;
        }

        let (op, seek, params) = match member.method {
            Method::Stored => (
                builtin("assemble@1"),
                SeekClass::Affine,
                AssembleParams {
                    segments: vec![Segment::BlobRange {
                        input_ix: 0,
                        offset: member.data_start,
                        len: member.comp_size,
                    }],
                }
                .encode()
                .map_err(|e| IngestError::Recipe(e.to_string()))?,
            ),
            Method::Deflate => (
                builtin("deflate-decompress@1"),
                SeekClass::Opaque,
                DeflateWindow {
                    offset: member.data_start,
                    len: member.comp_size,
                }
                .encode(),
            ),
        };
        let recipe = Recipe {
            op,
            inputs: vec![InputRef {
                hash: *zip_hash,
                role: None,
            }],
            outputs: vec![OutputRef {
                hash: tuple.blake3,
                size: member.uncomp_size,
                name: Some(member.name.clone()),
            }],
            params,
        };
        claims.push(MemberClaim {
            tuple,
            recipe: Some((recipe, seek)),
        });
    }
    Ok(claims)
}

/// Evaluate detectors against a whole buffered file; first match wins.
fn evaluate_detectors(
    store: &Store,
    detectors: &[Detector],
    bytes: &[u8],
    file_hash: &Blake3,
    notes: &mut Vec<String>,
) -> Result<Inside, IngestError> {
    let file_len = bytes.len() as u64;
    for detector in detectors {
        let Some(decision) = detector.evaluate(bytes) else {
            continue;
        };
        if decision.is_whole_file(file_len) || decision.is_empty() {
            return Ok(Inside::Opaque);
        }

        let variant = decision.apply(bytes);
        let mut hasher = AliasHasher::new();
        hasher.update(&variant);
        let tuple = hasher.finalize();

        if decision.operation != Operation::None {
            notes.push(format!(
                "detector {}: swap-operation recipe deferred until swap@1 params freeze \
                 (variant {} aliased only)",
                detector.name, tuple.blake3
            ));
            return Ok(Inside::Detector(Box::new(DetectorClaim {
                variant: tuple,
                derive: None,
                rebuild: None,
            })));
        }
        let role = format!("skipper:{}", detector.name);

        // Derive: variant = slice of the stored file.
        let derive_params = AssembleParams {
            segments: vec![Segment::BlobRange {
                input_ix: 0,
                offset: decision.start,
                len: decision.len(),
            }],
        }
        .encode()
        .map_err(|e| IngestError::Recipe(e.to_string()))?;
        let derive = Recipe {
            op: builtin("assemble@1"),
            inputs: vec![InputRef {
                hash: *file_hash,
                role: Some(role.clone()),
            }],
            outputs: vec![OutputRef {
                hash: tuple.blake3,
                size: decision.len(),
                name: None,
            }],
            params: derive_params,
        };

        // Rebuild: file = header blob + variant. Only for the common
        // prefix-header shape (decision reaches EOF); the header is a
        // real blob so it dedupes across dumps (docs/recipes.md).
        let mut rebuild = None;
        if decision.start > 0 && decision.end == file_len {
            let header = &bytes[..decision.start as usize];
            let mut h = AliasHasher::new();
            h.update(header);
            let header_tuple = h.finalize();
            store.put(StoreNs::Data, header_tuple.blake3, header)?;

            let rebuild_params = AssembleParams {
                segments: vec![
                    Segment::BlobRange {
                        input_ix: 0,
                        offset: 0,
                        len: decision.start,
                    },
                    Segment::BlobRange {
                        input_ix: 1,
                        offset: 0,
                        len: decision.len(),
                    },
                ],
            }
            .encode()
            .map_err(|e| IngestError::Recipe(e.to_string()))?;
            rebuild = Some((
                header_tuple,
                Recipe {
                    op: builtin("assemble@1"),
                    inputs: vec![
                        InputRef {
                            hash: header_tuple.blake3,
                            role: Some(role.clone()),
                        },
                        InputRef {
                            hash: tuple.blake3,
                            role: None,
                        },
                    ],
                    outputs: vec![OutputRef {
                        hash: *file_hash,
                        size: file_len,
                        name: None,
                    }],
                    params: rebuild_params,
                },
                SeekClass::Affine,
            ));
        }
        return Ok(Inside::Detector(Box::new(DetectorClaim {
            variant: tuple,
            derive: Some((derive, SeekClass::Affine)),
            rebuild,
        })));
    }
    Ok(Inside::Opaque)
}

/// One position of the sorted walk.
enum Step {
    File(PathBuf, fs::Metadata),
    Note(String),
    Failed(PathBuf, String),
}

/// The sorted walk as a resumable cursor: a stack of already-sorted
/// sibling lists. The writer interleaves walking with retiring, so the
/// walk yields one position at a time and never collects the corpus
/// first — D36's ten million small files are exactly the case that
/// would pay for a path vector.
struct Walk {
    stack: Vec<std::vec::IntoIter<PathBuf>>,
}

impl Walk {
    fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            stack: vec![roots.into_iter()],
        }
    }

    fn next(&mut self) -> Option<Step> {
        loop {
            let path = loop {
                let top = self.stack.last_mut()?;
                if let Some(path) = top.next() {
                    break path;
                }
                self.stack.pop();
            };
            let meta = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => return Some(Step::Failed(path, e.to_string())),
            };
            if meta.file_type().is_symlink() {
                return Some(Step::Note(format!("skipped symlink: {}", path.display())));
            }
            if meta.is_dir() {
                let mut entries: Vec<PathBuf> = match fs::read_dir(&path) {
                    Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
                    Err(e) => return Some(Step::Failed(path, e.to_string())),
                };
                entries.sort();
                self.stack.push(entries.into_iter());
                continue;
            }
            return Some(Step::File(path, meta));
        }
    }
}

/// Publish a recipe object (meta namespace) and index it as Verified —
/// shared by the ingest pass and refinement analyzers (both mint claims
/// about bytes they just hashed, D4). Index rows derive from the recipe
/// object ([`Db::index_recipe`]), never from caller-supplied tuples.
/// Idempotent by content address. Returns the recipe row id (existing
/// or new).
pub(crate) fn mint_recipe(
    store: &Store,
    db: &mut Db,
    recipe: &Recipe,
    seek: SeekClass,
) -> Result<i64, IngestError> {
    let encoded = recipe
        .encode()
        .map_err(|e| IngestError::Recipe(e.to_string()))?;
    let recipe_hash = Blake3::compute(&encoded);
    store.put(StoreNs::Meta, recipe_hash, encoded.as_slice())?;
    let recipe_blob_id = db.upsert_blob(
        &recipe_hash,
        Some(encoded.len() as u64),
        IndexNs::Meta,
        Residency::Resident,
    )?;
    if let Some(existing) = db.recipe_id_for_blob(recipe_blob_id)? {
        return Ok(existing); // re-mint of already-claimed content
    }
    let recipe_id = db.index_recipe(recipe_blob_id, recipe, seek, RecipeSource::LocalIngest)?;
    db.set_verify_state(recipe_id, datboi_index::VerifyAdvance::Verified, now_unix())?;
    Ok(recipe_id)
}

/// Load every detector XML in a directory; unparsable files are reported,
/// not fatal.
pub fn load_detectors(dir: &Path) -> (Vec<Detector>, Vec<(PathBuf, String)>) {
    let mut detectors = Vec::new();
    let mut errors = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => return (detectors, vec![(dir.to_owned(), e.to_string())]),
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        if path.extension().and_then(|e| e.to_str()) != Some("xml") {
            continue;
        }
        match fs::read(&path) {
            Ok(bytes) => match Detector::parse(&bytes) {
                Ok(d) => detectors.push(d),
                Err(e) => errors.push((path, e.to_string())),
            },
            Err(e) => errors.push((path, e.to_string())),
        }
    }
    (detectors, errors)
}

fn builtin(name_at_major: &str) -> Op {
    let (name, major) = name_at_major
        .split_once('@')
        .expect("builtin names are name@major");
    Op::Builtin {
        name: name.to_owned(),
        major: major.parse().expect("builtin major is numeric"),
    }
}

/// DEFLATE cannot expand by more than ~1032:1 (a 258-byte match coded
/// in as little as two bits), so a central directory declaring more
/// than that from its compressed bytes is lying and the member will
/// fail its size check below. That normally costs nothing — the
/// decoder simply runs dry — but `obao::compute` sizes its tree buffer
/// (~len/256) from the DECLARED length UP FRONT, before the lie can
/// surface. Members past this bound therefore skip ingest blessing
/// rather than let a fabricated size drive an allocation.
const MAX_DEFLATE_EXPANSION: u64 = 1032;

/// Does this member's derive route need an output outboard built here?
/// (D63 amendment.) Three conditions, all necessary:
///
/// - **DEFLATE only.** A STORED member's recipe is affine over the
///   container, so the D63 carve-out already serves every byte of it
///   verified — blessing it is the pure cost D63 rejected.
/// - **Bigger than one chunk group.** At or under 16 KiB the outboard
///   is empty by construction and absence IS the sidecar; this is
///   exactly why small ROMs read fine while larger ones returned
///   `MissingOutboard`.
/// - **A believable declaration** ([`MAX_DEFLATE_EXPANSION`]).
fn blesses_at_ingest(member: &zip::Member) -> bool {
    member.method == Method::Deflate
        && datboi_store_fs::obao::outboard_size(member.uncomp_size) > 0
        && member.uncomp_size
            <= member
                .comp_size
                .saturating_mul(MAX_DEFLATE_EXPANSION)
                .saturating_add(datboi_store_fs::obao::GROUP_BYTES)
}

/// Hash one member by streaming out of the stored container. Returns a
/// reason string (for the report) on any inconsistency — a lying central
/// directory must not produce a claim.
///
/// For a member whose route cannot take the D63 carve-out
/// ([`blesses_at_ingest`]) the second half of the pair is its bao
/// outboard, built in the SAME inflate that builds the alias tuple:
/// the bytes are already streaming past for D2's five hashers, so the
/// tree is one more consumer, never a second pass (D63 amendment).
fn hash_member<R: Read + Seek>(
    blob: &mut R,
    member: &zip::Member,
) -> Result<(AliasTuple, Option<Vec<u8>>), String> {
    blob.seek(SeekFrom::Start(member.data_start))
        .map_err(|e| e.to_string())?;
    let window = Window {
        inner: blob,
        remaining: member.comp_size,
    };
    let mut hasher = AliasHasher::new();
    // Bounded at declared+1: one extra byte proves the directory lied,
    // and a bomb-shaped member (tiny declared size, monstrous actual
    // inflation) costs declared-size work instead of full inflation.
    let cap = member.uncomp_size.saturating_add(1);
    if blesses_at_ingest(member) {
        return hash_and_bless_member(
            flate2::read::DeflateDecoder::new(window).take(cap),
            member,
            hasher,
        );
    }
    let counted = match member.method {
        Method::Stored => stream_into(window.take(cap), &mut hasher),
        Method::Deflate => stream_into(
            flate2::read::DeflateDecoder::new(window).take(cap),
            &mut hasher,
        ),
    }
    .map_err(|e| format!("member data unreadable: {e}"))?;
    if counted > member.uncomp_size {
        return Err(bomb_shaped(member));
    }
    if counted != member.uncomp_size {
        return Err(size_mismatch(member, counted));
    }
    Ok((hasher.finalize(), None))
}

/// The blessing variant of the pass above: the bao tree builder pulls,
/// [`TeeHash`] feeds the alias chain on the way past. Exactly one
/// inflate, two sets of hashers.
fn hash_and_bless_member(
    reader: impl Read,
    member: &zip::Member,
    mut hasher: AliasHasher,
) -> Result<(AliasTuple, Option<Vec<u8>>), String> {
    let mut tee = TeeHash {
        inner: reader,
        hasher: &mut hasher,
        count: 0,
    };
    // `compute` pulls exactly the declared length, so a short member
    // surfaces here as an I/O error, not as a wrong tree.
    let computed = datboi_store_fs::obao::compute(&mut tee, member.uncomp_size);
    let counted = tee.count;
    let (root, sidecar) = match computed {
        Ok(v) => v,
        // Keep the non-blessing path's diagnosis: a reader that ran dry
        // means the directory over-declared, which is a claim verdict,
        // not the I/O fault `compute` reports it as.
        Err(_) if counted < member.uncomp_size => {
            return Err(size_mismatch(member, counted));
        }
        Err(e) => return Err(format!("member data unreadable: {e}")),
    };
    // The `cap` take left room for exactly one byte past the
    // declaration; if it is there, the directory under-declared.
    let mut extra = [0u8; 1];
    match tee.read(&mut extra) {
        Ok(0) => {}
        Ok(_) => return Err(bomb_shaped(member)),
        Err(e) => return Err(format!("member data unreadable: {e}")),
    }
    let tuple = hasher.finalize();
    // Free correctness check: the obao root and the tuple's blake3 are
    // the same value computed two ways, over the same bytes, in the
    // same pass. They can only disagree if one of the two is broken.
    if root != tuple.blake3 {
        return Err(format!(
            "outboard root {root} disagrees with the member's blake3 {} — refusing claim",
            tuple.blake3
        ));
    }
    Ok((tuple, Some(sidecar)))
}

fn bomb_shaped(member: &zip::Member) -> String {
    format!(
        "member inflates past its declared {} bytes — bomb-shaped, refusing claim",
        member.uncomp_size
    )
}

fn size_mismatch(member: &zip::Member, counted: u64) -> String {
    format!(
        "central directory size mismatch: cd says {}, data yields {counted}",
        member.uncomp_size
    )
}

/// A reader that feeds every byte it yields into an [`AliasHasher`] on
/// the way past, counting as it goes — what lets one inflate drive both
/// the alias tuple and the bao tree (D63 amendment).
struct TeeHash<'a, R> {
    inner: R,
    hasher: &'a mut AliasHasher,
    count: u64,
}

impl<R: Read> Read for TeeHash<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Ok(n) => {
                    self.hasher.update(&buf[..n]);
                    self.count += n as u64;
                    return Ok(n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

fn stream_into(mut reader: impl Read, hasher: &mut AliasHasher) -> std::io::Result<u64> {
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => {
                hasher.update(&buf[..n]);
                total += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// A bounded sequential window over an already-positioned reader.
struct Window<'a, R> {
    inner: &'a mut R,
    remaining: u64,
}

impl<R: Read> Read for Window<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let cap = usize::try_from(self.remaining.min(buf.len() as u64)).expect("bounded");
        let n = self.inner.read(&mut buf[..cap])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

pub(crate) fn read_head(file: &mut impl Read, head: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
