//! Unpacking a transport container (D123): make every member a resident
//! literal, then drop the archive's bytes.
//!
//! ## What a container is, and why the sniff decides it
//!
//! No dat names an archive. zip, 7z and rar are equally absent from
//! every dat, so from a dat's point of view all three are the same
//! object — packaging someone shipped the roms in. D123 makes retention
//! a choice rather than a property of the format, and this module is
//! the one implementation behind both doors: `datboi ingest --unpack`
//! at the front, `datboi unpack` for a corpus already inside.
//!
//! The index can only *narrow* the population
//! ([`Db::unpack_candidates_after`]: a resident blob that is the sole
//! input of a live recipe claiming outputs). It cannot decide
//! containerhood, because single-input recipes also describe D9
//! detector variants and every D111/D114/D115/D116 disc decomposition —
//! and those inputs are dat-named discs. So the verdict is the head
//! sniff, the SAME predicate ingest used to decide it was a container
//! in the first place. A second definition would drift, and the drift
//! here destroys bytes.
//!
//! ## Order is the whole correctness argument
//!
//! D122: two media, no shared transaction. Members are published to the
//! store, then recorded in the index, then the index is asked whether
//! every member it still expects out of this container is durable — and
//! only then does the container's file go. An interrupt at any point
//! leaves the members derivable:
//!
//! * before the index rows land — the bytes are on disk under rows that
//!   call them absent, which is D122's safe drift, and the container is
//!   untouched so the recipes still fire;
//! * after the rows, before the unlink — members resident, container
//!   resident, and a re-run is a no-op that drops the container;
//! * between the unlink and the residency flip — the one window that
//!   cannot be closed across a filesystem and SQLite. [`triage`] finds
//!   it (a row claiming `Resident` over bytes that are gone, with every
//!   member durable), finishes the flip, and counts it as reconciled.
//!
//! ## What survives the drop
//!
//! Everything except the archive's bytes. The container keeps its blob
//! row, its alias tuple and its `source_file` provenance ("these bytes
//! arrived as roms/pac.zip"), and it keeps its `container->member`
//! recipes, which after the drop are the only record tying a rom to the
//! archive it came in. Residency goes to `Absent`, never
//! `EvictedCovered`: there is no covering route and the enum must not
//! claim one. That also keeps the row out of GC's orphan predicate,
//! which requires `residency = 0`.
//!
//! ## Shape (D120/D121)
//!
//! A bounded pool of workers does the zip lane — parse, inflate,
//! `put_new`, build the tree — and touches no `Db`. The coordinator
//! owns the only `Db` handle and does every read before dispatch, and
//! every write after retirement. 7z/rar stay ON the coordinator for
//! D120's recorded reason: that path lazily builds and publishes wasm
//! state and already fans out internally (D89 batch pipes, a consumer
//! thread per member), so a container is parallel where it counts and
//! containers of those formats are the rare case.

use std::collections::VecDeque;
use std::io::Read;
use std::num::NonZeroUsize;
use std::sync::{Mutex, PoisonError, mpsc};

use datboi_core::alias::AliasTuple;
use datboi_core::hash::Blake3;
use datboi_index::{Db, Namespace as IndexNs, Residency};
use datboi_store_fs::{Namespace as StoreNs, Store};

use crate::zip::{self, Method};
use crate::{ExFormat, Extracted, ExtractorRt, IngestError, Window, ensure_extractor_rt, now_unix};

/// Dispatched-but-unfinished ceiling. A zip job buffers nothing whole
/// (`put_new` streams to a temp file through a 64 KiB window), so there
/// is no byte budget to bound it with — per D120's amendment the lanes
/// charged nothing are bounded by the worker count and a queue cap
/// instead. Disk, which IS spent unboundedly, has its own D56 guard.
const QUEUE_CAP: usize = 64;

/// Candidate rows read per keyset page.
const PAGE: usize = 1024;

/// Free space a container's extraction must leave behind, over and
/// above the member bytes it is about to write (D56, same slack the
/// materialize path uses).
const HEADROOM_SLACK: u64 = 64 << 20;

/// What one container gave up: the members now durable in the store,
/// and the (member, reason) pairs it refused. A refusal is not fatal on
/// its own — it becomes one when the index still expects that member,
/// which is what blocks the drop.
pub type Yield = (Vec<Extracted>, Vec<(String, String)>);

/// Which transport format a container's head says it is — the same
/// three sniffs ingest runs, and the only thing that licenses a drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zip,
    SevenZ,
    Rar,
}

/// The head sniff. `None` means "the index called this a single-input
/// route, but the bytes are not transport" — a detector variant, a disc
/// decomposition, anything else. Refused, never guessed at.
#[must_use]
pub fn sniff(head: &[u8]) -> Option<Kind> {
    if zip::looks_like_zip(head) {
        Some(Kind::Zip)
    } else if crate::archive::looks_like_7z(head) {
        Some(Kind::SevenZ)
    } else if crate::archive::looks_like_rar(head) {
        Some(Kind::Rar)
    } else {
        None
    }
}

/// Inflate every member of a STORED zip container into the store as a
/// resident literal. Store only — no `Db`, which is what lets N of
/// these run at once.
///
/// Each member is read bounded at its declared length plus one byte:
/// one extra byte proves the central directory lied, and a bomb-shaped
/// member costs declared-size work instead of full inflation. A member
/// whose bytes disagree with the directory is REFUSED rather than
/// claimed — its bytes did land in the CAS (streaming means the size is
/// learned last), where they are content-addressed and unreferenced,
/// i.e. GC fodder rather than corruption. The refusal then blocks the
/// container's drop, because the index still expects that member.
///
/// The outboard is built for every member. Unpacked members are the
/// ONLY copy of their bytes — the container that could re-derive them
/// is about to be destroyed — so a verified read (D49) is worth one
/// extra pass over a file that was just written and is still warm.
///
/// # Errors
/// Store I/O, and a container whose central directory will not parse.
pub fn zip_members_into_store(store: &Store, container: &Blake3) -> Result<Yield, IngestError> {
    let mut blob = store
        .get(StoreNs::Data, container)?
        .ok_or_else(|| IngestError::Recipe("container vanished from the store".into()))?;
    let parsed = zip::parse_members(&mut blob)?;
    let mut skips: Vec<(String, String)> = parsed
        .skipped
        .into_iter()
        .map(|s| (s.name, s.reason.to_owned()))
        .collect();
    let mut out = Vec::with_capacity(parsed.members.len());
    for (ix, member) in parsed.members.iter().enumerate() {
        match store_zip_member(store, &mut blob, member) {
            Ok(aliases) => out.push(Extracted {
                ix: u32::try_from(ix).unwrap_or(u32::MAX),
                name: member.name.clone(),
                hash: aliases.blake3,
                aliases,
            }),
            Err(reason) => skips.push((member.name.clone(), reason)),
        }
    }
    Ok((out, skips))
}

/// One member, streamed out of the container and into the store —
/// `put_new` for the bytes, `ensure_obao` for the tree.
pub(crate) fn store_zip_member(
    store: &Store,
    blob: &mut datboi_store_fs::Blob,
    member: &zip::Member,
) -> Result<AliasTuple, String> {
    use std::io::{Seek, SeekFrom};

    blob.seek(SeekFrom::Start(member.data_start))
        .map_err(|e| e.to_string())?;
    let window = Window {
        inner: blob,
        remaining: member.comp_size,
    };
    // Declared + 1: the extra byte is how an under-declaring directory
    // is caught, and the cap is how a bomb costs declared-size work.
    let cap = member.uncomp_size.saturating_add(1);
    let reader: Box<dyn Read> = match member.method {
        Method::Stored => Box::new(window.take(cap)),
        Method::Deflate => Box::new(flate2::read::DeflateDecoder::new(window).take(cap)),
    };
    let (hash, aliases, _) = store
        .put_new(StoreNs::Data, reader)
        .map_err(|e| format!("member data unreadable: {e}"))?;
    if aliases.size > member.uncomp_size {
        return Err(crate::bomb_shaped(member));
    }
    if aliases.size != member.uncomp_size {
        return Err(crate::size_mismatch(member, aliases.size));
    }
    store
        .ensure_obao(StoreNs::Data, &hash)
        .map_err(|e| format!("building the member's tree: {e}"))?;
    Ok(aliases)
}

/// Record what a durable member MEANS in the index: a resident,
/// alias-indexed, verified blob. The bytes were hashed on the way in by
/// `put_new`, so `verified_at` is honestly earned here — unlike D122's
/// residency repair, which finds a file and may claim nothing else.
///
/// # Errors
/// Index failures.
pub fn record_member(db: &Db, member: &Extracted) -> Result<i64, IngestError> {
    let id = db.upsert_blob(
        &member.hash,
        Some(member.aliases.size),
        IndexNs::Data,
        Residency::Resident,
    )?;
    db.insert_aliases(id, &member.aliases)?;
    db.set_verified(id, now_unix())?;
    Ok(id)
}

/// Destroy a transport container's bytes, once its members are durable
/// (D123). The gate is [`Db::container_member_claims`]: every blob the
/// index still expects out of this container must satisfy `Store::has`.
/// Returns the bytes reclaimed, or `None` when the gate refused.
///
/// Unlike eviction this removes the outboard too (`remove_blob`, the
/// same call D73 orphan deletion uses): D49 rule 1 keeps a tree so an
/// evicted blob can still serve verified ranges through its rebuild
/// route, and this blob has no rebuild route — a tree over bytes
/// nothing can reconstruct is dead weight.
///
/// # Errors
/// Store or index failures.
pub fn drop_container(
    store: &Store,
    db: &Db,
    blob_id: i64,
    hash: &Blake3,
) -> Result<Option<u64>, IngestError> {
    for (_, member, _) in db.container_member_claims(blob_id)? {
        if !store.has(StoreNs::Data, &member) {
            return Ok(None);
        }
    }
    let bytes = store.len(StoreNs::Data, hash)?.unwrap_or(0);
    // The point of no return, ordered like eviction's: the file goes,
    // then the row. A crash between them leaves a row claiming Resident
    // over bytes that are gone — which `triage` recognises and finishes,
    // and which `scrub` reports honestly in the meantime.
    store.remove_blob(StoreNs::Data, hash)?;
    // Absent, not EvictedCovered: nothing covers these bytes. The row,
    // the aliases and the source_file provenance all stay.
    db.set_residency(blob_id, Residency::Absent)?;
    Ok(Some(bytes))
}

#[derive(Debug, Clone, Default)]
pub struct UnpackOptions {
    /// Worker count; 0 derives it from the machine (D120's convention).
    pub parallelism: usize,
    /// Decide and count, destroy nothing.
    pub dry_run: bool,
    /// Stop after selecting this many containers; 0 = no limit.
    pub limit: u64,
}

impl UnpackOptions {
    /// The worker count this config asks for, never zero.
    #[must_use]
    pub fn workers(&self) -> usize {
        if self.parallelism > 0 {
            return self.parallelism;
        }
        std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
    }
}

/// What one pass concluded. Counts and hex hashes only.
#[derive(Debug, Clone, Default)]
pub struct UnpackReport {
    /// [`Db::unpack_candidate_count`] read before the run — the number
    /// an operator can reproduce in `sqlite3`, and the number
    /// [`Self::examined`] must match for the walk to claim it saw
    /// everything (D121's completion lesson).
    pub population: u64,
    /// The same count after the run. A successful unpack removes its
    /// container from the population, so this is what is left.
    pub population_after: u64,
    /// Candidates triaged, before any verdict.
    pub examined: u64,
    /// The index called it a single-input route; the bytes are not zip,
    /// 7z or rar. A detector variant or a disc decomposition — refused.
    pub not_transport: u64,
    /// Rows repaired rather than acted on: an interrupted drop finished
    /// (bytes already gone, every member durable), or D122 drift.
    pub reconciled: u64,
    /// Containers this run would convert.
    pub selected: u64,
    /// Container bytes behind [`Self::selected`] — what a drop reclaims.
    pub selected_bytes: u64,
    /// Member bytes the index claims behind [`Self::selected`] — what
    /// residency costs. The other half of the bill, and on a rom corpus
    /// it is the bigger half.
    pub claimed_member_bytes: u64,
    /// Containers converted and dropped.
    pub unpacked: u64,
    /// Members made resident this run.
    pub members_resident: u64,
    /// Member bytes written this run.
    pub member_bytes: u64,
    /// Container bytes reclaimed this run.
    pub dropped_bytes: u64,
    /// Containers whose members are durable but whose index rows could
    /// not be written (a contended `Db`, D122). The bytes stand; the
    /// container was NOT dropped. A re-run reconciles.
    pub unrecorded: u64,
    /// Set when the D56 headroom guard stopped the run.
    pub out_of_room: bool,
    /// (member, reason) the container refused to give up, with the
    /// container's hash — these are what block a drop, so they are the
    /// first thing an operator needs.
    pub skipped_members: Vec<(String, String, String)>,
    /// (hex hash, error) in candidate order.
    pub failed: Vec<(String, String)>,
}

impl UnpackReport {
    /// Nothing refused.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }

    /// Did the walk visit every candidate the count twin promised?
    #[must_use]
    pub fn walked_it_all(&self) -> bool {
        self.examined == self.population
    }

    /// Containers still owed after this run.
    #[must_use]
    pub fn outstanding(&self) -> u64 {
        self.selected.saturating_sub(self.unpacked)
    }

    /// Everything the pass was given, it either did or accounted for.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.is_clean() && self.outstanding() == 0 && self.walked_it_all() && self.unrecorded == 0
    }
}

/// The coordinator's mutable half.
struct State<'p> {
    report: UnpackReport,
    /// Failures keyed by candidate sequence: the pool finishes out of
    /// order, and this is the one ordered output (D121).
    failures: Vec<(u64, String, String)>,
    progress: &'p mut dyn FnMut(&UnpackReport),
}

impl State<'_> {
    fn tick(&mut self) {
        (self.progress)(&self.report);
    }

    fn fail(&mut self, seq: u64, hash: &Blake3, err: &str) {
        self.failures.push((seq, hash.to_hex(), err.to_owned()));
    }
}

/// One container handed to a worker.
struct Job {
    seq: u64,
    blob_id: i64,
    hash: Blake3,
}

/// A worker's answer.
struct Done {
    seq: u64,
    blob_id: i64,
    hash: Blake3,
    result: Result<Yield, IngestError>,
}

/// Convert every retained transport container into resident members
/// (D123). The operator's command; nothing calls this on its own.
///
/// `progress` is called once per retired candidate with the report so
/// far — the pass runs for a long time on a real corpus and must not be
/// one silent block. Throttling is the CALLER's.
///
/// # Errors
/// Environmental index failures only. A container that refuses to
/// unpack is a `failed` entry, not an `Err` (D81).
pub fn unpack_corpus(
    store: &Store,
    db: &Db,
    opts: &UnpackOptions,
    progress: &mut dyn FnMut(&UnpackReport),
) -> Result<UnpackReport, IngestError> {
    let mut state = State {
        report: UnpackReport::default(),
        failures: Vec::new(),
        progress,
    };
    state.report.population = db.unpack_candidate_count()?;

    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let job_rx = Mutex::new(job_rx);
    let (done_tx, done_rx) = mpsc::channel::<Done>();

    let result = std::thread::scope(|scope| {
        for _ in 0..opts.workers() {
            let done_tx = done_tx.clone();
            let job_rx = &job_rx;
            scope.spawn(move || {
                loop {
                    // Held across the recv only.
                    let job = {
                        let rx = job_rx.lock().unwrap_or_else(PoisonError::into_inner);
                        rx.recv()
                    };
                    let Ok(job) = job else { return };
                    // A panicking worker must become one failed
                    // container, never a coordinator blocked forever on
                    // a result nobody will send.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        zip_members_into_store(store, &job.hash)
                    }))
                    .unwrap_or_else(|_| {
                        Err(IngestError::Worker(format!(
                            "unpacking {} panicked",
                            job.hash
                        )))
                    });
                    let done = Done {
                        seq: job.seq,
                        blob_id: job.blob_id,
                        hash: job.hash,
                        result,
                    };
                    if done_tx.send(done).is_err() {
                        return;
                    }
                }
            });
        }
        // Ours would keep the channel alive forever.
        drop(done_tx);
        let outcome = unpack_loop(store, db, opts, &job_tx, &done_rx, &mut state);
        drop(job_tx);
        outcome
    });
    result?;

    let State {
        mut report,
        mut failures,
        ..
    } = state;
    report.population_after = db.unpack_candidate_count()?;
    failures.sort_unstable_by_key(|(seq, _, _)| *seq);
    report.failed = failures
        .into_iter()
        .map(|(_, hash, err)| (hash, err))
        .collect();
    Ok(report)
}

/// The coordinator: page candidates, triage, dispatch zips, run the
/// component formats here (D120's placement), retire and drop. EVERY
/// `Db` read and write the pass makes happens on this thread.
fn unpack_loop(
    store: &Store,
    db: &Db,
    opts: &UnpackOptions,
    jobs: &mpsc::Sender<Job>,
    done: &mpsc::Receiver<Done>,
    state: &mut State<'_>,
) -> Result<(), IngestError> {
    let mut extractor: Option<ExtractorRt> = None;
    let mut next_seq = 0u64;
    let mut outstanding = 0usize;
    let mut page: VecDeque<(i64, Blake3, u64)> = VecDeque::new();
    let mut cursor = 0i64;
    let mut walking = true;

    loop {
        while walking && outstanding < QUEUE_CAP {
            if page.is_empty() {
                page = db.unpack_candidates_after(cursor, PAGE)?.into();
                let Some((last, _, _)) = page.back() else {
                    walking = false;
                    break;
                };
                cursor = *last;
            }
            let Some((blob_id, hash, size)) = page.pop_front() else {
                continue;
            };
            // Every candidate takes a sequence number, skipped ones
            // included: it is the failure list's sort key.
            let seq = next_seq;
            next_seq += 1;
            state.report.examined += 1;
            let Some(kind) = triage(store, db, blob_id, &hash, state)? else {
                state.tick();
                continue;
            };
            let member_bytes: u64 = db
                .container_member_claims(blob_id)?
                .iter()
                .map(|(_, _, size)| *size)
                .fold(0, u64::saturating_add);
            state.report.selected += 1;
            state.report.selected_bytes += size;
            state.report.claimed_member_bytes += member_bytes;
            if opts.limit != 0 && state.report.selected >= opts.limit {
                walking = false;
                page.clear();
            }
            if opts.dry_run {
                state.tick();
                continue;
            }
            // D56: N containers extract concurrently, so the check is
            // per job. The first refusal stops staging and drains, so a
            // full disk is one reported failure and not one per
            // container.
            if let Some(have) = store.available_bytes()?
                && have < member_bytes.saturating_add(HEADROOM_SLACK)
            {
                state.report.out_of_room = true;
                state.fail(
                    seq,
                    &hash,
                    &format!(
                        "store filesystem has {have} byte(s) free; unpacking needs \
                         {member_bytes} plus slack"
                    ),
                );
                walking = false;
                page.clear();
                break;
            }
            if kind == Kind::Zip {
                if jobs.send(Job { seq, blob_id, hash }).is_ok() {
                    outstanding += 1;
                }
                continue;
            }
            // 7z/rar on the coordinator (D120): the component path
            // builds and publishes wasm state and already fans out
            // internally.
            let fmt = if kind == Kind::SevenZ {
                ExFormat::SevenZ
            } else {
                ExFormat::Rar
            };
            let result = component_members(&mut extractor, store, db, fmt, &hash);
            retire(store, db, seq, blob_id, &hash, result, state);
            state.tick();
        }

        if outstanding == 0 {
            if !walking {
                return Ok(());
            }
            continue;
        }
        let Ok(d) = done.recv() else {
            let detail = format!("unpack pool died with {outstanding} container(s) in flight");
            state.fail(next_seq, &Blake3([0u8; 32]), &detail);
            return Ok(());
        };
        outstanding -= 1;
        let unrecorded_before = state.report.unrecorded;
        retire(store, db, d.seq, d.blob_id, &d.hash, d.result, state);
        if state.report.unrecorded != unrecorded_before {
            // An index write failed (D122): the member bytes are
            // durable and the container was NOT dropped, so nothing is
            // lost — but staging stops, the queue drains, and the run
            // does not call itself complete.
            walking = false;
            page.clear();
        }
        state.tick();
    }
}

/// Extract a 7z/rar container's members on the coordinator, publishing
/// the pinned component's blob row the first time it is loaded.
fn component_members(
    extractor: &mut Option<ExtractorRt>,
    store: &Store,
    db: &Db,
    fmt: ExFormat,
    hash: &Blake3,
) -> Result<Yield, IngestError> {
    let published = ensure_extractor_rt(extractor, store, fmt).map_err(IngestError::Recipe)?;
    if let Some((component, len)) = published {
        db.upsert_blob(&component, Some(len), IndexNs::Data, Residency::Resident)?;
    }
    let rt = extractor.as_ref().expect("ensure_extractor_rt first");
    let members = rt
        .members_into_store(store, fmt, hash)
        .map_err(IngestError::Recipe)?;
    Ok((members, Vec::new()))
}

/// Everything the coordinator decides about one candidate before a
/// worker may see it. `Ok(None)` is a counted skip.
fn triage(
    store: &Store,
    db: &Db,
    blob_id: i64,
    hash: &Blake3,
    state: &mut State<'_>,
) -> Result<Option<Kind>, IngestError> {
    if !store.has(StoreNs::Data, hash) {
        // The row says Resident over bytes that are gone. Either an
        // unpack was interrupted in its one unclosable window (the
        // unlink landed, the residency flip did not), or this is real
        // loss. Only the first is repairable, and only when every
        // member is durable — which is exactly the gate a drop needed
        // in the first place, so ask it again.
        if drop_container(store, db, blob_id, hash)?.is_some() {
            state.report.reconciled += 1;
        } else {
            state.fail(
                state.report.examined,
                hash,
                "bytes are gone and the index called them resident, but members are missing \
                 too — this is loss, not an interrupted unpack; run `datboi scrub`",
            );
        }
        return Ok(None);
    }
    let mut blob = store
        .get(StoreNs::Data, hash)?
        .ok_or_else(|| IngestError::Recipe("container vanished from the store".into()))?;
    let mut head = [0u8; 8];
    let read = crate::read_head(&mut blob, &mut head)
        .map_err(|e| IngestError::Recipe(format!("reading {hash}'s head: {e}")))?;
    // THE verdict. The index only narrowed the population; single-input
    // recipes also describe D9 detector variants and D111/D114/D115/D116
    // disc decompositions, whose inputs are dat-named discs. Anything
    // the sniff does not call transport is refused.
    let Some(kind) = sniff(&head[..read]) else {
        state.report.not_transport += 1;
        return Ok(None);
    };
    Ok(Some(kind))
}

/// Record a container's extracted members, then drop the container.
/// The index write comes FIRST and the unlink LAST — that ordering is
/// the whole interrupt argument (module docs).
fn retire(
    store: &Store,
    db: &Db,
    seq: u64,
    blob_id: i64,
    hash: &Blake3,
    result: Result<Yield, IngestError>,
    state: &mut State<'_>,
) {
    let (members, skips) = match result {
        Ok(v) => v,
        Err(e) => {
            state.fail(seq, hash, &e.to_string());
            return;
        }
    };
    for (member, reason) in skips {
        state
            .report
            .skipped_members
            .push((hash.to_hex(), member, reason));
    }
    let mut bytes = 0u64;
    for member in &members {
        if let Err(e) = record_member(db, member) {
            // D122: the bytes are durable, and propagating would throw
            // away every finished job behind this one. Count it, keep
            // the container, and let a re-run reconcile.
            state.report.unrecorded += 1;
            state.fail(seq, hash, &e.to_string());
            return;
        }
        bytes = bytes.saturating_add(member.aliases.size);
    }
    match drop_container(store, db, blob_id, hash) {
        Ok(Some(reclaimed)) => {
            state.report.unpacked += 1;
            state.report.members_resident += members.len() as u64;
            state.report.member_bytes += bytes;
            state.report.dropped_bytes += reclaimed;
        }
        // The gate refused: something the index still expects out of
        // this container is not in the store, so the container stays.
        // The members that DID come out are resident and keep their
        // rows — nothing is undone, the archive is simply not dropped.
        Ok(None) => state.fail(
            seq,
            hash,
            "kept: the index still expects members this container did not produce",
        ),
        Err(e) => state.fail(seq, hash, &e.to_string()),
    }
}
