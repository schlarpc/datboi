//! The M1 ingest pipeline (docs/90-roadmap.md, D35/D40): walk sources,
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

pub mod analyzers;
pub mod archive;
pub mod refine;
pub mod zip;

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use datboi_core::alias::{AliasHasher, AliasTuple};
use datboi_core::assemble::{AssembleParams, Segment};
use datboi_core::cbor::{self, Value};
use datboi_core::hash::Blake3;
use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe, World};
use datboi_formats::skipper::{Detector, Operation};
use datboi_index::recipes::NewRecipe;
use datboi_index::{Db, Namespace as IndexNs, OpKind, RecipeSource, Residency, SeekClass};
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

/// CBOR key for the extractor member index in an `ex-unrar/extract`
/// recipe's params (D58); mirrors exec's `EXTRACTOR_PARAM_MEMBER_IX`.
const EXTRACTOR_PARAM_MEMBER_IX: u64 = 1;

/// Streaming buffer size for member hashing.
const CHUNK: usize = 64 * 1024;

/// deflate-decompress@1 params: a window into input 0 (`{1: offset,
/// 2: len}`, strict canonical CBOR). One recipe per member instead of a
/// slice-recipe + intermediate blob per member — at MAME scale the row
/// economy matters, and the op owns its params schema
/// (docs/70-recipes.md).
const DEFLATE_PARAM_OFFSET: u64 = 1;
const DEFLATE_PARAM_LEN: u64 = 2;

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
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            skipper_cap: 256 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Default)]
pub struct IngestReport {
    pub files_scanned: usize,
    /// Rescan-cache hits: path+mtime+size unchanged, nothing re-read.
    pub files_unchanged: usize,
    pub files_stored: usize,
    pub files_already_present: usize,
    /// CHD v5 files whose declared internal sha1 was recorded.
    pub chd_v5: usize,
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

/// Lazily-built rar extractor state (D58): the wasm host, the compiled
/// `ex-unrar` component, and whether its bytes have been published into
/// the store this sweep (recipes pin it by hash).
struct ExtractorRt {
    host: ExtractorHost,
    component: datboi_runtime::extractor::ExtractorComponent,
    published: bool,
}

pub struct Ingester<'a> {
    store: &'a Store,
    db: &'a mut Db,
    detectors: &'a [Detector],
    config: IngestConfig,
    /// Built on the first rar container encountered (avoids the wasm engine
    /// cost when a sweep has no rar).
    extractor: Option<ExtractorRt>,
    /// Resident-blob ids accumulated across `record_resident_blob`
    /// calls; drained into `IngestReport::fresh_blobs` per run.
    fresh: Vec<i64>,
    /// Source-identity override for staged single-file ingests (web
    /// uploads): the `source_file` key becomes this name instead of
    /// the throwaway staging path, so provenance reads
    /// "roms/pack.zip", re-uploads update one row instead of minting
    /// dead staging-path rows, and the mtime+size check still defeats
    /// false rescan-cache hits. Single-file semantics only — a
    /// directory walk under one name would collide keys.
    source_name: Option<String>,
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
            source_name: None,
        }
    }

    #[must_use]
    pub fn with_config(mut self, config: IngestConfig) -> Self {
        self.config = config;
        self
    }

    /// Name the source (see the field docs): callers staging one file
    /// under a throwaway path give provenance the identity the USER
    /// knows.
    #[must_use]
    pub fn with_source_name(mut self, name: impl Into<String>) -> Self {
        self.source_name = Some(name.into());
        self
    }

    /// Ingest files and directory trees. Directories walk in sorted order
    /// for deterministic reports; symlinks are skipped.
    pub fn ingest(&mut self, paths: &[impl AsRef<Path>]) -> IngestReport {
        let mut report = IngestReport::default();
        for path in paths {
            self.walk(path.as_ref(), &mut report);
        }
        report.fresh_blobs = std::mem::take(&mut self.fresh);
        report
    }

    fn walk(&mut self, path: &Path, report: &mut IngestReport) {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                report.errors.push((path.to_owned(), e.to_string()));
                return;
            }
        };
        if meta.file_type().is_symlink() {
            report
                .notes
                .push(format!("skipped symlink: {}", path.display()));
            return;
        }
        if meta.is_dir() {
            let mut entries: Vec<PathBuf> = match fs::read_dir(path) {
                Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
                Err(e) => {
                    report.errors.push((path.to_owned(), e.to_string()));
                    return;
                }
            };
            entries.sort();
            for entry in entries {
                self.walk(&entry, report);
            }
            return;
        }
        report.files_scanned += 1;
        if let Err(e) = self.process_file(path, &meta, report) {
            report.errors.push((path.to_owned(), e.to_string()));
        }
    }

    fn process_file(
        &mut self,
        path: &Path,
        meta: &fs::Metadata,
        report: &mut IngestReport,
    ) -> Result<(), IngestError> {
        let canonical = fs::canonicalize(path).map_err(|e| IngestError::io(path, e))?;
        let key = self
            .source_name
            .clone()
            .unwrap_or_else(|| canonical.to_string_lossy().into_owned());
        let mtime_ns = mtime_ns(meta);
        let size = meta.len();

        if self
            .db
            .lookup_unchanged_source(&key, mtime_ns, size)?
            .is_some()
        {
            report.files_unchanged += 1;
            return Ok(());
        }

        let source = File::open(&canonical).map_err(|e| IngestError::io(&canonical, e))?;
        let (hash, aliases, outcome) = self.store.put_new(StoreNs::Data, source)?;
        match outcome {
            PutOutcome::Stored => report.files_stored += 1,
            PutOutcome::AlreadyPresent => report.files_already_present += 1,
        }
        let blob_id = self.record_resident_blob(&hash, &aliases)?;

        // Look inside the *stored* bytes (verifies what we published).
        let mut blob = self
            .store
            .get(StoreNs::Data, &hash)?
            .expect("just published");
        // One head read serves both container sniffs (zip magic is 4 bytes,
        // a CHD v5 header is 124).
        let mut head = [0u8; datboi_formats::chd::CHD_V5_HEADER_LEN];
        let head_len = read_head(&mut blob, &mut head).map_err(|e| IngestError::io(path, e))?;
        if let Some(chd) = datboi_formats::chd::parse_header(&head[..head_len]) {
            self.process_chd(path, blob_id, &chd, report)?;
        } else if zip::looks_like_zip(&head[..head_len]) {
            if let Err(e) = self.process_zip(path, &hash, blob_id, &mut blob, report) {
                report.errors.push((path.to_owned(), e.to_string()));
            }
        } else if archive::looks_like_7z(&head[..head_len]) {
            if let Err(e) = self.process_7z(&mut blob, report) {
                report.errors.push((path.to_owned(), e));
            }
        } else if archive::looks_like_rar(&head[..head_len]) {
            if let Err(e) = self.process_rar(&hash, blob_id, report) {
                report.errors.push((path.to_owned(), e));
            }
        } else if !self.detectors.is_empty() {
            if size <= self.config.skipper_cap {
                blob.seek(SeekFrom::Start(0))
                    .map_err(|e| IngestError::io(path, e))?;
                let mut bytes = Vec::with_capacity(size as usize);
                blob.read_to_end(&mut bytes)
                    .map_err(|e| IngestError::io(path, e))?;
                self.process_detectors(&bytes, &hash, blob_id, report)?;
            } else {
                report.skipper_skipped_large += 1;
            }
        }

        // Last, so a crash before this point re-processes the file.
        self.db
            .upsert_source_file(&key, mtime_ns, size, Some(blob_id), now_unix())?;
        Ok(())
    }

    /// Claim every supported member of a stored zip container.
    /// CHD v5: record the header's declared internal sha1 (the identity
    /// MAME disk claims reference). Header-only — the declaration grades as
    /// `probable` in audit (D44) until a decompressing verify exists (M3).
    fn process_chd(
        &mut self,
        path: &Path,
        blob_id: i64,
        chd: &datboi_formats::chd::ChdHeader,
        report: &mut IngestReport,
    ) -> Result<(), IngestError> {
        match chd {
            datboi_formats::chd::ChdHeader::V5(v5) => {
                self.db.insert_declared_chd_sha1(blob_id, &v5.sha1)?;
                report.chd_v5 += 1;
                if v5.has_parent() {
                    report.notes.push(format!(
                        "{}: delta CHD (has a parent); recorded, but standalone rebuild is impossible",
                        path.display()
                    ));
                }
            }
            datboi_formats::chd::ChdHeader::Unsupported { version } => {
                report.notes.push(format!(
                    "{}: CHD v{version} header not supported (v5 only); stored as opaque bytes",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    /// Extract every 7z member into the CAS (see the archive module docs
    /// for why extraction, not claims: no LZMA-class rebuild transform
    /// exists yet, so the container stays literal and the members become
    /// first-class resident blobs).
    fn process_7z(&mut self, blob: &mut File, report: &mut IngestReport) -> Result<(), String> {
        let store = self.store;
        let mut stored: Vec<(datboi_core::alias::AliasTuple, String)> = Vec::new();
        archive::extract_7z(blob, |name, reader| {
            let (_, aliases, _) = store
                .put_new(StoreNs::Data, reader)
                .map_err(|e| format!("storing member {name:?}: {e}"))?;
            stored.push((aliases, name.to_owned()));
            Ok(())
        })?;
        for (aliases, _name) in &stored {
            self.record_resident_blob(&aliases.blake3, aliases)
                .map_err(|e| e.to_string())?;
            report.members_extracted += 1;
        }
        Ok(())
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
        container_blob_id: i64,
        report: &mut IngestReport,
    ) -> Result<(), String> {
        self.ensure_extractor()?;
        let members = self.extractor_enumerate(container_hash)?;

        for member in &members {
            // Decode the member inside the sandbox, streaming into the CAS
            // (hash computed on the way in). Neither the container nor the
            // member is ever whole in memory.
            let (member_hash, aliases) =
                self.extractor_extract_into_store(container_hash, member)?;
            if aliases.size != member.size {
                // The mismatched bytes already landed in the CAS (streaming
                // means we learn the size last); they are content-addressed
                // and unreferenced — GC fodder, not corruption. Refuse the
                // archive before minting any claim to them.
                return Err(format!(
                    "member {:?}: extractor produced {} bytes, header claims {}",
                    member.name, aliases.size, member.size
                ));
            }
            let member_blob_id = self
                .record_resident_blob(&member_hash, &aliases)
                .map_err(|e| e.to_string())?;

            // Mint the container→member derive recipe (makes the member
            // evictable). Empty members need no recipe (nothing to rebuild).
            if member.size > 0 {
                let params = cbor::encode(&Value::Map(vec![(
                    EXTRACTOR_PARAM_MEMBER_IX,
                    Value::Uint(u64::from(member.ix)),
                )]))
                .map_err(|e| e.to_string())?;
                let recipe = Recipe {
                    op: Op::Wasm {
                        component: self.extractor_component_hash()?,
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
                mint_recipe(
                    self.store,
                    self.db,
                    &recipe,
                    "ex-unrar/extract",
                    SeekClass::Opaque,
                    &[(0, container_blob_id, None)],
                    &[(0, member_blob_id, member.size, Some(&member.name))],
                )
                .map_err(|e| e.to_string())?;
            }
            report.members_extracted += 1;
        }
        Ok(())
    }

    /// Lazily build the extractor host + compile the pinned component, and
    /// publish the component into the store+index once per sweep (recipes
    /// pin it by hash, so a later replay can load it).
    fn ensure_extractor(&mut self) -> Result<(), String> {
        if self.extractor.is_none() {
            let host =
                ExtractorHost::new(datboi_runtime::Limits::default()).map_err(|e| e.to_string())?;
            let component = host.load(EX_UNRAR_WASM).map_err(|e| e.to_string())?;
            self.extractor = Some(ExtractorRt {
                host,
                component,
                published: false,
            });
        }
        if !self.extractor.as_ref().expect("just set").published {
            let hash = Blake3::compute(EX_UNRAR_WASM);
            self.store
                .put(StoreNs::Data, hash, EX_UNRAR_WASM)
                .map_err(|e| e.to_string())?;
            self.db
                .upsert_blob(
                    &hash,
                    Some(EX_UNRAR_WASM.len() as u64),
                    IndexNs::Data,
                    Residency::Resident,
                )
                .map_err(|e| e.to_string())?;
            self.extractor.as_mut().expect("just set").published = true;
        }
        Ok(())
    }

    fn extractor_component_hash(&self) -> Result<Blake3, String> {
        Ok(Blake3::compute(EX_UNRAR_WASM))
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
        container: &Blake3,
    ) -> Result<Vec<datboi_runtime::extractor::Member>, String> {
        let rt = self.extractor.as_ref().expect("ensure_extractor first");
        rt.host
            .enumerate(&rt.component, self.container_random(container)?)
            .map_err(|e| e.to_string())
    }

    /// Decode one member into the CAS: the extractor pushes into a bounded
    /// pipe on its own thread while `put_new` hashes and stores the pull
    /// side. An extractor failure surfaces to the reader as an error (never
    /// a clean EOF), so `put_new` deletes its temp and publishes nothing.
    fn extractor_extract_into_store(
        &self,
        container: &Blake3,
        member: &datboi_runtime::extractor::Member,
    ) -> Result<(Blake3, AliasTuple), String> {
        let rt = self.extractor.as_ref().expect("ensure_extractor first");
        let archive = self.container_random(container)?;
        let (w, r, h) = pipe::pipe();
        let ix = member.ix;
        std::thread::scope(|s| {
            s.spawn(move || {
                let _finished = h.finish_on_drop();
                if let Err(e) = rt.host.extract(&rt.component, archive, ix, Box::new(w)) {
                    h.fail(format!("extractor failed: {e}"));
                }
            });
            let (hash, aliases, _) = self
                .store
                .put_new(StoreNs::Data, r)
                .map_err(|e| format!("storing member {:?}: {e}", member.name))?;
            Ok((hash, aliases))
        })
    }

    fn process_zip(
        &mut self,
        path: &Path,
        zip_hash: &Blake3,
        zip_blob_id: i64,
        blob: &mut File,
        report: &mut IngestReport,
    ) -> Result<(), IngestError> {
        let parsed = zip::parse_members(blob)?;
        for skip in parsed.skipped {
            report
                .member_skips
                .push((path.to_owned(), skip.name, skip.reason.to_owned()));
        }
        for member in parsed.members {
            let tuple = match hash_member(blob, &member) {
                Ok(t) => t,
                Err(reason) => {
                    report
                        .member_skips
                        .push((path.to_owned(), member.name, reason));
                    continue;
                }
            };
            let member_blob_id = self.record_absent_blob(&tuple)?;

            if member.uncomp_size == 0 {
                // The empty output needs no recipe (assemble@1 rejects
                // empty segment lists by design); store the empty literal
                // so the identity is grounded.
                self.store
                    .put(StoreNs::Data, tuple.blake3, std::io::empty())?;
                self.db
                    .upsert_blob(&tuple.blake3, Some(0), IndexNs::Data, Residency::Resident)?;
                report.members_claimed += 1;
                continue;
            }

            let (op_name, seek, params) = match member.method {
                Method::Stored => (
                    "assemble@1",
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
                    "deflate-decompress@1",
                    SeekClass::Opaque,
                    cbor::encode(&Value::Map(vec![
                        (DEFLATE_PARAM_OFFSET, Value::Uint(member.data_start)),
                        (DEFLATE_PARAM_LEN, Value::Uint(member.comp_size)),
                    ]))
                    .map_err(|e| IngestError::Recipe(e.to_string()))?,
                ),
            };
            let recipe = Recipe {
                op: builtin(op_name),
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
            self.record_recipe(
                &recipe,
                op_name,
                seek,
                &[(0, zip_blob_id, None)],
                &[(0, member_blob_id, member.uncomp_size, Some(&member.name))],
            )?;
            report.members_claimed += 1;
        }
        Ok(())
    }

    /// Evaluate detectors against a whole buffered file; first match wins.
    fn process_detectors(
        &mut self,
        bytes: &[u8],
        file_hash: &Blake3,
        file_blob_id: i64,
        report: &mut IngestReport,
    ) -> Result<(), IngestError> {
        let file_len = bytes.len() as u64;
        for detector in self.detectors {
            let Some(decision) = detector.evaluate(bytes) else {
                continue;
            };
            if decision.is_whole_file(file_len) || decision.is_empty() {
                return Ok(());
            }
            report.detector_hits += 1;

            let variant = decision.apply(bytes);
            let mut hasher = AliasHasher::new();
            hasher.update(&variant);
            let tuple = hasher.finalize();
            let variant_blob_id = self.record_absent_blob(&tuple)?;

            if decision.operation != Operation::None {
                report.notes.push(format!(
                    "detector {}: swap-operation recipe deferred until swap@1 params freeze \
                     (variant {} aliased only)",
                    detector.name, tuple.blake3
                ));
                return Ok(());
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
            self.record_recipe(
                &derive,
                "assemble@1",
                SeekClass::Affine,
                &[(0, file_blob_id, Some(role.as_str()))],
                &[(0, variant_blob_id, decision.len(), None)],
            )?;

            // Rebuild: file = header blob + variant. Only for the common
            // prefix-header shape (decision reaches EOF); the header is a
            // real blob so it dedupes across dumps (docs/70-recipes.md).
            if decision.start > 0 && decision.end == file_len {
                let header = &bytes[..decision.start as usize];
                let mut h = AliasHasher::new();
                h.update(header);
                let header_tuple = h.finalize();
                self.store.put(StoreNs::Data, header_tuple.blake3, header)?;
                let header_hash = header_tuple.blake3;
                let header_blob_id = self.record_resident_blob(&header_hash, &header_tuple)?;

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
                let rebuild = Recipe {
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
                };
                self.record_recipe(
                    &rebuild,
                    "assemble@1",
                    SeekClass::Affine,
                    &[
                        (0, header_blob_id, Some(role.as_str())),
                        (1, variant_blob_id, None),
                    ],
                    &[(0, file_blob_id, file_len, None)],
                )?;
            }
            return Ok(());
        }
        Ok(())
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
    fn record_recipe(
        &mut self,
        recipe: &Recipe,
        op_name: &str,
        seek: SeekClass,
        inputs: &[(u32, i64, Option<&str>)],
        outputs: &[(u32, i64, u64, Option<&str>)],
    ) -> Result<(), IngestError> {
        mint_recipe(self.store, self.db, recipe, op_name, seek, inputs, outputs)?;
        Ok(())
    }
}

/// Publish a recipe object (meta namespace) and index it as Verified —
/// shared by the ingest pass and refinement analyzers (both mint claims
/// about bytes they just hashed, D4). Idempotent by content address.
/// Returns the recipe row id (existing or new).
pub(crate) fn mint_recipe(
    store: &Store,
    db: &mut Db,
    recipe: &Recipe,
    op_name: &str,
    seek: SeekClass,
    inputs: &[(u32, i64, Option<&str>)],
    outputs: &[(u32, i64, u64, Option<&str>)],
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
    if let Some(existing) = recipe_row_id(db, recipe_blob_id)? {
        return Ok(existing); // re-mint of already-claimed content
    }
    let recipe_id = db.insert_recipe(&NewRecipe {
        blob_id: recipe_blob_id,
        op_kind: match recipe.op {
            datboi_core::recipe::Op::Builtin { .. } => OpKind::Builtin,
            datboi_core::recipe::Op::Wasm { .. } => OpKind::Wasm,
        },
        op_name,
        seek_class: seek,
        source: RecipeSource::LocalIngest,
        inputs,
        outputs,
    })?;
    db.set_verify_state(
        recipe_id,
        datboi_index::VerifyState::Verified,
        now_unix(),
        None,
    )?;
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

/// Idempotency guard for re-mints: the recipe row is UNIQUE on its blob.
/// (Queries through `Db::cache()`; no direct rusqlite dependency needed.)
fn recipe_row_id(db: &Db, recipe_blob_id: i64) -> Result<Option<i64>, datboi_index::IndexError> {
    let mut stmt = db
        .cache()
        .prepare_cached("SELECT recipe_id FROM recipe WHERE blob_id = ?1")?;
    let mut rows = stmt.query((recipe_blob_id,))?;
    Ok(rows.next()?.map(|row| row.get(0)).transpose()?)
}

/// Hash one member by streaming out of the stored container. Returns a
/// reason string (for the report) on any inconsistency — a lying central
/// directory must not produce a claim.
fn hash_member(blob: &mut File, member: &zip::Member) -> Result<AliasTuple, String> {
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
    let counted = match member.method {
        Method::Stored => stream_into(window.take(cap), &mut hasher),
        Method::Deflate => stream_into(
            flate2::read::DeflateDecoder::new(window).take(cap),
            &mut hasher,
        ),
    }
    .map_err(|e| format!("member data unreadable: {e}"))?;
    if counted > member.uncomp_size {
        return Err(format!(
            "member inflates past its declared {} bytes — bomb-shaped, refusing claim",
            member.uncomp_size
        ));
    }
    if counted != member.uncomp_size {
        return Err(format!(
            "central directory size mismatch: cd says {}, data yields {counted}",
            member.uncomp_size
        ));
    }
    Ok(hasher.finalize())
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
struct Window<'a> {
    inner: &'a mut File,
    remaining: u64,
}

impl Read for Window<'_> {
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

fn read_head(file: &mut File, head: &mut [u8]) -> std::io::Result<usize> {
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
