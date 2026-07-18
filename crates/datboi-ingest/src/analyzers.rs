//! Real analyzers for the refinement fixpoint (M3, D45). Each one is a
//! pure function of bytes × its identity tag; discoveries are minted as
//! ordinary recipes and the provenance row records why (or why not).

use std::io::BufReader;

use datboi_core::assemble::{AssembleParams, Segment};
use datboi_core::hash::Blake3;
use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe, World};
use datboi_index::{AnalysisOutcome, Db, Namespace as IndexNs, Residency, SeekClass, SweepItem};
use datboi_store_fs::{Namespace as StoreNs, Store};

use crate::refine::{AnalysisResult, Analyzer, Logical, Pulse, TickReader, analyzer_tag};

/// FastCDC parameters (D3 strategy ladder, rung 3): gear hash, NC level
/// 2, 64 KiB / 256 KiB / 1 MiB — tuned for disc images. The values are
/// baked into the analyzer identity: retuning mints a NEW analyzer whose
/// sweep re-covers the corpus, while old recipes stay valid forever
/// (chunker identity is provenance, not replay input — docs/recipes.md).
pub const CHUNK_MIN: usize = 64 * 1024;
pub const CHUNK_AVG: usize = 256 * 1024;
pub const CHUNK_MAX: usize = 1024 * 1024;

/// Blobs below this aren't worth a chunk recipe (they ARE a chunk).
/// Provisional policy, molten with the rest of the ingest-policy config
/// vocabulary (open-questions.md).
pub const CHUNK_THRESHOLD: u64 = 4 * 1024 * 1024;

/// The canonical `xf-preflate` component (D53), embedded so the analyzer
/// can publish it as an ordinary CAS blob the moment it mints the first
/// recipe that pins it. Nix-built, spliced in at compile time via
/// `DATBOI_COMPONENTS_DIR` (D66); the runtime gate exercises the same
/// artifact.
pub const XF_PREFLATE_WASM: &[u8] = include_bytes!(concat!(
    env!("DATBOI_COMPONENTS_DIR"),
    "/datboi_xf_preflate.wasm"
));

/// Compressed-window size fed to the splitter per step. Baked into the
/// analyzer identity: retuning changes frame boundaries (and so the
/// corrections blobs), which is a NEW discovery pass, never a broken old
/// recipe.
const SPLIT_WINDOW: usize = 4 * 1024 * 1024;

/// Per-frame plaintext ceiling handed to preflate (bounds rebuild memory;
/// comfortably under the guest's 64 MiB MAX_FRAME guard).
const SPLIT_PLAINTEXT_LIMIT: usize = 32 * 1024 * 1024;

/// Skeleton ceiling: zip structure (headers + central dir + any
/// non-deflate bytes) beyond this means the container isn't worth a
/// rebuild recipe.
const SKELETON_LIMIT: u64 = 64 * 1024 * 1024;

/// The analyzer names a manual sweep accepts (canonical form) — one
/// list for the CLI `sweep` verb and the daemon's `POST /v1/sweep`
/// available-list. Broader than [`crate::refine::FAMILIES`]: `narc` is
/// its own sweep analyzer but shares the `nds` config family, so the two
/// vocabularies are deliberately distinct.
pub const SWEEP_ANALYZERS: &[&str] = &["noop", "chunk", "preflate", "ecm", "nds", "narc"];

/// Construct a sweep analyzer by name — the shared factory both the CLI
/// and `POST /v1/sweep` build from, so the accepted vocabulary lives in
/// ONE place (D96). Each family's canonical name and its CLI aliases
/// resolve; an unknown name is `None` (the caller owns the error message,
/// spelling out [`SWEEP_ANALYZERS`]).
#[must_use]
pub fn analyzer_for(name: &str) -> Option<Box<dyn Analyzer>> {
    Some(match name {
        "noop" | "noop/1" => Box::new(crate::refine::NoopAnalyzer),
        "chunk" | "fastcdc" => Box::new(ChunkAnalyzer),
        "preflate" | "preflate-split" => Box::new(PreflateZipAnalyzer::new()),
        "ecm" => Box::new(EcmAnalyzer::new()),
        "nds" | "nds-split" => Box::new(NdsAnalyzer),
        "narc" | "narc-split" => Box::new(NarcAnalyzer),
        _ => return None,
    })
}

/// Wild-zip rebuild discovery + minting (D24/D45/D53): split every
/// DEFLATE member into plaintext + a framed preflate corrections blob,
/// mint one `xf-preflate recreate` recipe per member and one `assemble@1`
/// recipe reassembling the container from a literal skeleton + the
/// rebuilt streams. After licensing (D25 replay) the container and the
/// member streams evict; what stays resident is the plaintext (the bytes
/// dats actually name, deduping against raw ingests) plus corrections at
/// ~0.002–3% of plaintext.
///
/// Failures are D48 negatives, recorded once, forever: preflate cleanly
/// errors on compressors outside its models (7-Zip's deflate — see the
/// open-questions entry), and those containers stay literal (D24).
pub struct PreflateZipAnalyzer {
    component_published: bool,
}

impl PreflateZipAnalyzer {
    /// Window parameters are part of the identity on purpose.
    const VERSIONED_NAME: &'static str = "preflate-split-0.7.6-w4m-p32m/1";

    #[must_use]
    pub fn new() -> Self {
        Self {
            component_published: false,
        }
    }

    /// blake3 of the embedded component — the hash recreate recipes pin.
    #[must_use]
    pub fn component_hash() -> Blake3 {
        Blake3::compute(XF_PREFLATE_WASM)
    }

    /// Publish the component into the store + index once per sweep.
    fn ensure_component(&mut self, store: &Store, db: &mut Db) -> Result<i64, String> {
        let hash = Self::component_hash();
        if !self.component_published {
            store
                .put(StoreNs::Data, hash, XF_PREFLATE_WASM)
                .map_err(|e| e.to_string())?;
            self.component_published = true;
        }
        db.upsert_blob(
            &hash,
            Some(XF_PREFLATE_WASM.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .map_err(|e| e.to_string())
    }
}

impl Default for PreflateZipAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// One member's split products, already stored.
struct MemberSplit {
    /// blake3 of the member's raw deflate bytes — the recreate output.
    stream_hash: Blake3,
    /// Framed corrections blob (stored, resident).
    corrections_hash: Blake3,
    corrections_len: u64,
    /// Member plaintext (stored, resident, alias-indexed).
    plaintext_hash: Blake3,
    plaintext_len: u64,
}

/// Streaming split driver: pulls compressed bytes from the container,
/// walks `PreflateStreamProcessor` window by window, and exposes the
/// produced plaintext as a `Read` so `Store::put_new` can ingest it in
/// the same single pass. Corrections frames accumulate on the side
/// (~0.002–3% of plaintext). Deterministic split failures land in
/// `fail`, distinguishable from real I/O errors.
struct SplitReader<'a> {
    src: &'a mut datboi_store_fs::Blob,
    remaining_comp: u64,
    window: Vec<u8>,
    processor: preflate_rs::PreflateStreamProcessor,
    stream_hasher: blake3::Hasher,
    corrections: Vec<u8>,
    pending: Vec<u8>,
    pending_pos: usize,
    done: bool,
    fail: Option<String>,
}

impl<'a> SplitReader<'a> {
    fn new(src: &'a mut datboi_store_fs::Blob, comp_size: u64) -> Self {
        let config = preflate_rs::PreflateConfig {
            // Self-verifying split: preflate recompresses each window and
            // compares before we ever mint a claim.
            verify_compression: true,
            plain_text_limit: SPLIT_PLAINTEXT_LIMIT,
            ..preflate_rs::PreflateConfig::default()
        };
        Self {
            src,
            remaining_comp: comp_size,
            window: Vec::new(),
            processor: preflate_rs::PreflateStreamProcessor::new(&config),
            stream_hasher: blake3::Hasher::new(),
            corrections: Vec::new(),
            pending: Vec::new(),
            pending_pos: 0,
            done: false,
            fail: None,
        }
    }

    fn split_failed(&mut self, msg: String) -> std::io::Error {
        self.fail = Some(msg.clone());
        std::io::Error::other(msg)
    }

    /// Feed windows until the next frame of plaintext exists (or the
    /// stream completes).
    fn advance(&mut self) -> std::io::Result<()> {
        use std::io::Read as _;
        loop {
            // Top up the compressed window.
            let want = SPLIT_WINDOW.saturating_sub(self.window.len());
            let take = u64::try_from(want)
                .expect("window fits u64")
                .min(self.remaining_comp);
            if take > 0 {
                let start = self.window.len();
                self.window
                    .resize(start + usize::try_from(take).expect("bounded"), 0);
                self.src.read_exact(&mut self.window[start..])?;
                self.stream_hasher.update(&self.window[start..]);
                self.remaining_comp -= take;
            }
            match self.processor.decompress(&self.window) {
                Ok(r) => {
                    if r.compressed_size == 0 && !self.processor.is_done() {
                        return Err(self.split_failed("split made no progress".into()));
                    }
                    let pt = self.processor.plain_text().text().to_vec();
                    self.processor.shrink_to_dictionary();
                    let (Ok(pt_len), Ok(corr_len)) =
                        (u32::try_from(pt.len()), u32::try_from(r.corrections.len()))
                    else {
                        return Err(self.split_failed("frame exceeds u32".into()));
                    };
                    self.corrections.extend_from_slice(&pt_len.to_le_bytes());
                    self.corrections.extend_from_slice(&corr_len.to_le_bytes());
                    self.corrections.extend_from_slice(&r.corrections);
                    self.window.drain(..r.compressed_size);
                    self.pending = pt;
                    self.pending_pos = 0;
                    if self.processor.is_done() {
                        if !self.window.is_empty() || self.remaining_comp > 0 {
                            return Err(self.split_failed(format!(
                                "deflate stream ended {} byte(s) before the declared member size",
                                self.window.len() as u64 + self.remaining_comp
                            )));
                        }
                        self.done = true;
                    }
                    return Ok(());
                }
                Err(e)
                    if e.exit_code() == preflate_rs::ExitCode::ShortRead
                        && self.remaining_comp > 0 => {}
                Err(e) => {
                    return Err(self.split_failed(format!("preflate split failed: {e}")));
                }
            }
        }
    }
}

impl std::io::Read for SplitReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pending_pos == self.pending.len() {
            if self.done {
                return Ok(0);
            }
            self.advance()?;
        }
        let n = (self.pending.len() - self.pending_pos).min(buf.len());
        buf[..n].copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + n]);
        self.pending_pos += n;
        Ok(n)
    }
}

impl Analyzer for PreflateZipAnalyzer {
    fn name(&self) -> &'static str {
        Self::VERSIONED_NAME
    }

    fn family(&self) -> &'static str {
        "preflate"
    }

    fn id(&self) -> Blake3 {
        analyzer_tag(Self::VERSIONED_NAME)
    }

    fn analyze(
        &mut self,
        item: &SweepItem,
        bytes: &Logical<'_, '_>,
        store: &Store,
        db: &mut Db,
        pulse: &mut dyn Pulse,
    ) -> Result<AnalysisResult, String> {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = bytes.open(item, db, pulse)?;
        let container_size = item
            .size
            .or_else(|| file.byte_len().ok())
            .ok_or("container size unknown")?;
        let mut head = [0u8; 4];
        let n = file.read(&mut head).map_err(|e| e.to_string())?;
        if !crate::zip::looks_like_zip(&head[..n]) {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some("not a zip container".into()),
            });
        }
        // A parse failure is a CONCLUSION about the bytes — they will
        // never change — so it settles as Negative (D81). This used to
        // `Err`, and one zip-magic blob with no end-of-central-directory
        // record re-errored on every 30-minute ambient sweep forever.
        let parsed = match crate::zip::parse_members(&mut file) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Ok(AnalysisResult {
                    outcome: AnalysisOutcome::Negative,
                    detail: Some(format!("zip parse failed: {e}; container stays literal")),
                });
            }
        };
        if !parsed.skipped.is_empty() {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                // D24: unsupported members leave the container literal.
                detail: Some(format!(
                    "{} member(s) outside the supported subset; container stays literal",
                    parsed.skipped.len()
                )),
            });
        }
        let deflate_members: Vec<&crate::zip::Member> = parsed
            .members
            .iter()
            .filter(|m| m.method == crate::zip::Method::Deflate)
            .collect();
        if deflate_members.is_empty() {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some("no deflate members to rebuild".into()),
            });
        }
        // Member data ranges must be sane: ordered, disjoint, in-bounds.
        let mut ranges: Vec<(u64, u64, usize)> = deflate_members
            .iter()
            .enumerate()
            .map(|(ix, m)| (m.data_start, m.comp_size, ix))
            .collect();
        ranges.sort_unstable();
        let mut prev_end = 0u64;
        for &(start, len, _) in &ranges {
            if start < prev_end || start.checked_add(len).is_none_or(|e| e > container_size) {
                return Ok(AnalysisResult {
                    outcome: AnalysisOutcome::Negative,
                    detail: Some("overlapping or out-of-bounds member ranges".into()),
                });
            }
            prev_end = start + len;
        }

        // ---- split members: PARTIAL COVERAGE (refined from
        // all-or-nothing after real-corpus evidence — a single estimator
        // failure out of hundreds must not condemn the container). A
        // member that refuses to split simply stays literal INSIDE the
        // skeleton; the D24 tax shrinks to exactly the refusing bytes.
        let mut covered: Vec<(u64, u64, MemberSplit)> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        for &(start, len, ix) in &ranges {
            file.seek(SeekFrom::Start(start))
                .map_err(|e| e.to_string())?;
            let mut reader = SplitReader::new(&mut file, len);
            // The split is the long haul (minutes for disc-sized
            // members): plaintext flowing into the store is the
            // heartbeat that keeps this item's lease alive (D71).
            let put = store.put_new(StoreNs::Data, TickReader::new(&mut reader, &mut *pulse));
            match put {
                Ok((plaintext_hash, aliases, _)) => {
                    let plaintext_id = db
                        .upsert_blob(
                            &plaintext_hash,
                            Some(aliases.size),
                            IndexNs::Data,
                            Residency::Resident,
                        )
                        .map_err(|e| e.to_string())?;
                    // The plaintext is the byte sequence dats name: index
                    // the full alias tuple so audit sees it.
                    db.insert_aliases(plaintext_id, &aliases)
                        .map_err(|e| e.to_string())?;
                    let corrections_hash = Blake3::compute(&reader.corrections);
                    store
                        .put(
                            StoreNs::Data,
                            corrections_hash,
                            reader.corrections.as_slice(),
                        )
                        .map_err(|e| e.to_string())?;
                    let stream_hash = Blake3(*reader.stream_hasher.finalize().as_bytes());
                    covered.push((
                        start,
                        len,
                        MemberSplit {
                            stream_hash,
                            corrections_hash,
                            corrections_len: reader.corrections.len() as u64,
                            plaintext_hash,
                            plaintext_len: aliases.size,
                        },
                    ));
                }
                Err(e) => {
                    // Deterministic split refusals feed the D48 record;
                    // real I/O stays an analyzer error (retryable).
                    if let Some(msg) = reader.fail.take() {
                        failures.push(format!("{:?}: {msg}", deflate_members[ix].name));
                    } else {
                        return Err(e.to_string());
                    }
                }
            }
        }
        if covered.is_empty() {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                // D24: no member splits, so the container stays literal.
                detail: Some(format!(
                    "no member splits; container stays literal. {}",
                    summarize_failures(&failures)
                )),
            });
        }
        let skeleton_len = container_size - covered.iter().map(|&(_, len, _)| len).sum::<u64>();
        if skeleton_len > SKELETON_LIMIT {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some(format!(
                    "skeleton would be {skeleton_len} bytes (> {SKELETON_LIMIT}); container stays literal"
                )),
            });
        }

        // ---- skeleton: every byte outside SPLIT member data, in order
        // (refused members ride along as literal skeleton ranges) ----
        let mut skeleton = Vec::with_capacity(usize::try_from(skeleton_len).unwrap_or(0));
        let mut gaps: Vec<(u64, u64)> = Vec::new(); // (container offset, len)
        let mut prev_end = 0u64;
        for &(start, len, _) in &covered {
            if start > prev_end {
                gaps.push((prev_end, start - prev_end));
            }
            prev_end = start + len;
        }
        if container_size > prev_end {
            gaps.push((prev_end, container_size - prev_end));
        }
        for &(off, len) in &gaps {
            file.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
            let mut piece = vec![0u8; usize::try_from(len).map_err(|e| e.to_string())?];
            file.read_exact(&mut piece).map_err(|e| e.to_string())?;
            skeleton.extend_from_slice(&piece);
        }
        let skeleton_hash = Blake3::compute(&skeleton);
        store
            .put(StoreNs::Data, skeleton_hash, skeleton.as_slice())
            .map_err(|e| e.to_string())?;
        db.upsert_blob(
            &skeleton_hash,
            Some(skeleton.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .map_err(|e| e.to_string())?;

        // ---- mint: one recreate recipe per split member ----
        self.ensure_component(store, db)?;
        let component_hash = Self::component_hash();
        for &(_, stream_len, ref split) in &covered {
            db.upsert_blob(
                &split.corrections_hash,
                Some(split.corrections_len),
                IndexNs::Data,
                Residency::Resident,
            )
            .map_err(|e| e.to_string())?;
            db.upsert_blob(
                &split.plaintext_hash,
                Some(split.plaintext_len),
                IndexNs::Data,
                Residency::Resident,
            )
            .map_err(|e| e.to_string())?;
            // The stream itself: known hash, no local bytes — it lives
            // inside the container until eviction, and the recipe is its
            // route thereafter.
            db.upsert_blob(
                &split.stream_hash,
                Some(stream_len),
                IndexNs::Data,
                Residency::Absent,
            )
            .map_err(|e| e.to_string())?;
            let recipe = Recipe {
                op: Op::Wasm {
                    component: component_hash,
                    world: World::Transform1,
                    export: "recreate".into(),
                },
                inputs: vec![
                    InputRef {
                        hash: split.corrections_hash,
                        role: Some("skeleton".into()),
                    },
                    InputRef {
                        hash: split.plaintext_hash,
                        role: None,
                    },
                ],
                outputs: vec![OutputRef {
                    hash: split.stream_hash,
                    size: stream_len,
                    name: None,
                }],
                params: Vec::new(),
            };
            crate::mint_recipe(store, db, &recipe, SeekClass::Opaque).map_err(|e| e.to_string())?;
        }

        // ---- mint: the container assemble over skeleton + streams ----
        let mut inputs = vec![InputRef {
            hash: skeleton_hash,
            role: Some("skeleton".into()),
        }];
        for (_, _, split) in &covered {
            inputs.push(InputRef {
                hash: split.stream_hash,
                role: None,
            });
        }
        let mut segments: Vec<Segment> = Vec::new();
        let mut skel_off = 0u64;
        let mut prev_end = 0u64;
        for (range_ix, (start, len, _)) in covered.iter().enumerate() {
            let (start, len) = (*start, *len);
            if start > prev_end {
                segments.push(Segment::BlobRange {
                    input_ix: 0,
                    offset: skel_off,
                    len: start - prev_end,
                });
                skel_off += start - prev_end;
            }
            segments.push(Segment::BlobRange {
                input_ix: u32::try_from(range_ix + 1).expect("member count fits u32"),
                offset: 0,
                len,
            });
            prev_end = start + len;
        }
        if container_size > prev_end {
            segments.push(Segment::BlobRange {
                input_ix: 0,
                offset: skel_off,
                len: container_size - prev_end,
            });
        }
        let recipe = Recipe {
            op: Op::Builtin {
                name: "assemble".into(),
                major: 1,
            },
            inputs,
            outputs: vec![OutputRef {
                hash: item.hash,
                size: container_size,
                name: None,
            }],
            params: AssembleParams { segments }
                .encode()
                .map_err(|e| e.to_string())?,
        };
        crate::mint_recipe(store, db, &recipe, SeekClass::Affine).map_err(|e| e.to_string())?;

        let corr_total: u64 = covered.iter().map(|(_, _, s)| s.corrections_len).sum();
        let pt_total: u64 = covered.iter().map(|(_, _, s)| s.plaintext_len).sum();
        let refused = if failures.is_empty() {
            String::new()
        } else {
            format!(
                "; {} member(s) stay literal in the skeleton ({})",
                failures.len(),
                summarize_failures(&failures)
            )
        };
        Ok(AnalysisResult {
            outcome: AnalysisOutcome::Positive,
            detail: Some(format!(
                "rebuildable: {}/{} member(s) split; plaintext {pt_total} B, corrections {corr_total} B, skeleton {} B{refused}",
                covered.len(),
                deflate_members.len(),
                skeleton.len()
            )),
        })
    }
}

/// First few split-refusal notes, count of the rest — provenance detail,
/// not a log dump.
fn summarize_failures(failures: &[String]) -> String {
    const SHOW: usize = 3;
    let mut s = failures
        .iter()
        .take(SHOW)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    if failures.len() > SHOW {
        s.push_str(&format!("; +{} more", failures.len() - SHOW));
    }
    s
}

/// Content-defined chunking analyzer: splits big opaque blobs into
/// FastCDC chunks stored as ordinary blobs, and mints one `assemble@1`
/// concat recipe reproducing the original. The recipe makes the original
/// evictable (D25 after replay) and the chunks dedupe across similar
/// blobs — the M3 shrink primitive for the long tail.
pub struct ChunkAnalyzer;

impl ChunkAnalyzer {
    /// Versioned identity: parameters are part of the name on purpose.
    const VERSIONED_NAME: &'static str = "fastcdc-v2020-nc2-64k-256k-1m/1";
}

impl Analyzer for ChunkAnalyzer {
    fn name(&self) -> &'static str {
        Self::VERSIONED_NAME
    }

    fn family(&self) -> &'static str {
        "chunk"
    }

    fn id(&self) -> Blake3 {
        analyzer_tag(Self::VERSIONED_NAME)
    }

    fn analyze(
        &mut self,
        item: &SweepItem,
        bytes: &Logical<'_, '_>,
        store: &Store,
        db: &mut Db,
        pulse: &mut dyn Pulse,
    ) -> Result<AnalysisResult, String> {
        let Some(size) = item
            .size
            .or_else(|| store.len(StoreNs::Data, &item.hash).ok().flatten())
        else {
            return Err("blob size unknown".into());
        };
        if size < CHUNK_THRESHOLD {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some(format!("below {CHUNK_THRESHOLD}-byte chunking threshold")),
            });
        }
        // Chunking mints RESIDENT chunks, so it only makes sense over a
        // resident literal — chunking an absent (grounded) blob would
        // MATERIALIZE it, the opposite of the dedup goal. Checked before
        // bytes open, so an absent item never pays a spill.
        let residency = db
            .blob_by_hash(&item.hash)
            .map_err(|e| e.to_string())?
            .map(|row| row.residency);
        if residency != Some(Residency::Resident) {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some("not a resident literal — chunking would materialize it".into()),
            });
        }
        // D59, rank-7 amended (D91): chunking's job is making big
        // literals evictable via cross-image dedup. A blob with a REAL
        // rebuild route — one that grounds it from OTHER retained bytes —
        // is already covered; chunking adds I/O + metadata for nothing.
        // But the old has-any-recipe test MISPREDICTED D91 grounding-leaf
        // pieces: a piece carries a `container→piece` recipe row, yet its
        // container grounds via this very piece, so it is route-LESS to
        // the D21 fixpoint and its cross-variant near-misses are exactly
        // what CDC should dedup. `is_covered_by_others` (grounded without
        // this blob's own literal) draws that line precisely.
        if db
            .is_covered_by_others(item.blob_id)
            .map_err(|e| e.to_string())?
        {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                // D59: an existing grounding route already covers this.
                detail: Some("already covered by a grounding route".into()),
            });
        }
        let file = bytes.open(item, db, pulse)?;

        // One streaming pass: chunk, store each chunk, build segments.
        let mut inputs: Vec<InputRef> = Vec::new();
        let mut segments: Vec<Segment> = Vec::new();
        let mut chunk_ids: Vec<(Blake3, u64)> = Vec::new();
        // Chunking a big image is a full sequential read: the bytes
        // themselves are the lease heartbeat (D71).
        let chunker = fastcdc::v2020::StreamCDC::with_level(
            BufReader::new(TickReader::new(file, pulse)),
            CHUNK_MIN,
            CHUNK_AVG,
            CHUNK_MAX,
            fastcdc::v2020::Normalization::Level2,
        );
        for chunk in chunker {
            let chunk = chunk.map_err(|e| format!("chunking failed: {e}"))?;
            let hash = Blake3::compute(&chunk.data);
            store
                .put(StoreNs::Data, hash, chunk.data.as_slice())
                .map_err(|e| e.to_string())?;
            chunk_ids.push((hash, chunk.data.len() as u64));
        }
        if chunk_ids.len() < 2 {
            // A single chunk would be a self-referential recipe (output
            // == input) — no decomposition happened.
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some("indivisible: content is a single chunk".into()),
            });
        }
        for (ix, (hash, len)) in chunk_ids.iter().enumerate() {
            db.upsert_blob(hash, Some(*len), IndexNs::Data, Residency::Resident)
                .map_err(|e| e.to_string())?;
            inputs.push(InputRef {
                hash: *hash,
                role: None,
            });
            segments.push(Segment::BlobRange {
                input_ix: u32::try_from(ix).expect("chunk count fits u32"),
                offset: 0,
                len: *len,
            });
        }

        let recipe = Recipe {
            op: Op::Builtin {
                name: "assemble".into(),
                major: 1,
            },
            inputs,
            outputs: vec![OutputRef {
                hash: item.hash,
                size,
                name: None,
            }],
            params: AssembleParams { segments }
                .encode()
                .map_err(|e| e.to_string())?,
        };
        crate::mint_recipe(store, db, &recipe, SeekClass::Affine).map_err(|e| e.to_string())?;

        Ok(AnalysisResult {
            outcome: AnalysisOutcome::Positive,
            detail: Some(format!("chunked into {} pieces", chunk_ids.len())),
        })
    }
}

/// The canonical `xf-ecm` component, embedded like xf-preflate's.
pub const XF_ECM_WASM: &[u8] = include_bytes!(concat!(
    env!("DATBOI_COMPONENTS_DIR"),
    "/datboi_xf_ecm.wasm"
));

/// CD sector regeneration discovery (the ECM idea; M3's last analyzer).
/// Scans a blob on the 2352-byte grid; every sector that REGENERATES
/// BIT-EXACTLY (verify-at-discovery, via the same crate the component is
/// built from) is stripped of its sync/EDC/ECC; everything else stays a
/// literal run. One recipe, no container assemble: the recreate output
/// IS the whole image. ~12.8% direct savings on PSX-era bins, and the
/// stripped payload chunks/dedupes better downstream.
pub struct EcmAnalyzer {
    component_published: bool,
}

impl EcmAnalyzer {
    /// Grid policy is part of the identity: a resync-capable v2 would be
    /// a new analyzer.
    const VERSIONED_NAME: &'static str = "ecm-ecma130-2352grid/1";

    #[must_use]
    pub fn new() -> Self {
        Self {
            component_published: false,
        }
    }

    /// blake3 of the embedded component — the hash recreate recipes pin.
    #[must_use]
    pub fn component_hash() -> Blake3 {
        Blake3::compute(XF_ECM_WASM)
    }

    fn ensure_component(&mut self, store: &Store, db: &mut Db) -> Result<i64, String> {
        let hash = Self::component_hash();
        if !self.component_published {
            store
                .put(StoreNs::Data, hash, XF_ECM_WASM)
                .map_err(|e| e.to_string())?;
            self.component_published = true;
        }
        db.upsert_blob(
            &hash,
            Some(XF_ECM_WASM.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .map_err(|e| e.to_string())
    }
}

impl Default for EcmAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// Streaming ECM splitter: walks the blob on the sector grid, emits
/// stripped bytes as a `Read` (driving `Store::put_new` in one pass),
/// and accumulates run-length layout records on the side.
struct EcmSplitReader<'a> {
    src: &'a mut datboi_store_fs::Blob,
    remaining: u64,
    records: Vec<datboi_xf_ecm::LayoutRecord>,
    sectors: [u64; 4], // by kind; [0] counts literal BYTES
    pending: Vec<u8>,
    pending_pos: usize,
}

impl<'a> EcmSplitReader<'a> {
    fn new(src: &'a mut datboi_store_fs::Blob, size: u64) -> Self {
        Self {
            src,
            remaining: size,
            records: Vec::new(),
            sectors: [0; 4],
            pending: Vec::new(),
            pending_pos: 0,
        }
    }

    fn push_run(&mut self, kind: u8, count: u32) {
        if let Some(last) = self.records.last_mut()
            && last.kind == kind
            && let Some(merged) = last.count.checked_add(count)
        {
            last.count = merged;
        } else {
            self.records
                .push(datboi_xf_ecm::LayoutRecord { kind, count });
        }
    }

    fn advance(&mut self) -> std::io::Result<()> {
        use std::io::Read as _;
        let take = self.remaining.min(datboi_xf_ecm::SECTOR as u64);
        if take == 0 {
            return Ok(());
        }
        let mut raw = vec![0u8; usize::try_from(take).expect("sector-bounded")];
        self.src.read_exact(&mut raw)?;
        self.remaining -= take;
        match datboi_xf_ecm::classify_sector(&raw) {
            Some((kind, stripped)) => {
                self.push_run(kind, 1);
                self.sectors[usize::from(kind)] += 1;
                self.pending = stripped;
            }
            None => {
                self.push_run(0, u32::try_from(raw.len()).expect("sector-bounded"));
                self.sectors[0] += raw.len() as u64;
                self.pending = raw;
            }
        }
        self.pending_pos = 0;
        Ok(())
    }
}

impl std::io::Read for EcmSplitReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pending_pos == self.pending.len() {
            if self.remaining == 0 {
                return Ok(0);
            }
            self.advance()?;
        }
        let n = (self.pending.len() - self.pending_pos).min(buf.len());
        buf[..n].copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + n]);
        self.pending_pos += n;
        Ok(n)
    }
}

impl Analyzer for EcmAnalyzer {
    fn name(&self) -> &'static str {
        Self::VERSIONED_NAME
    }

    fn family(&self) -> &'static str {
        "ecm"
    }

    fn id(&self) -> Blake3 {
        analyzer_tag(Self::VERSIONED_NAME)
    }

    fn analyze(
        &mut self,
        item: &SweepItem,
        bytes: &Logical<'_, '_>,
        store: &Store,
        db: &mut Db,
        pulse: &mut dyn Pulse,
    ) -> Result<AnalysisResult, String> {
        use std::io::Read;

        let mut file = bytes.open(item, db, pulse)?;
        let size = item
            .size
            .or_else(|| file.byte_len().ok())
            .ok_or("blob size unknown")?;
        if size < datboi_xf_ecm::SECTOR as u64 {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some("smaller than one raw CD sector".into()),
            });
        }
        // Cheap gate: a bin's first sector starts with the sync pattern.
        let mut head = [0u8; 12];
        file.read_exact(&mut head).map_err(|e| e.to_string())?;
        if head != datboi_xf_ecm::SYNC {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some("no CD sync pattern at offset 0".into()),
            });
        }
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;

        let mut reader = EcmSplitReader::new(&mut file, size);
        // Full sector-grid walk of the image: same heartbeat rule as
        // the other streaming analyzers (D71).
        let (stripped_hash, aliases, _) = store
            .put_new(StoreNs::Data, TickReader::new(&mut reader, pulse))
            .map_err(|e| e.to_string())?;
        let regenerable: u64 = reader.sectors[1] + reader.sectors[2] + reader.sectors[3];
        if regenerable == 0 {
            // Sync at 0 but nothing verified — scrambled or nonstandard.
            // The stripped blob equals the original; harmless orphan row.
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some("sync present but no sector regenerates bit-exactly".into()),
            });
        }
        let mut layout = Vec::with_capacity(reader.records.len() * 5);
        for r in &reader.records {
            layout.extend_from_slice(&datboi_xf_ecm::encode_record(*r));
        }
        let layout_hash = Blake3::compute(&layout);
        store
            .put(StoreNs::Data, layout_hash, layout.as_slice())
            .map_err(|e| e.to_string())?;

        let component_hash = Self::component_hash();
        self.ensure_component(store, db)?;
        db.upsert_blob(
            &stripped_hash,
            Some(aliases.size),
            IndexNs::Data,
            Residency::Resident,
        )
        .map_err(|e| e.to_string())?;
        db.upsert_blob(
            &layout_hash,
            Some(layout.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .map_err(|e| e.to_string())?;
        let recipe = Recipe {
            op: Op::Wasm {
                component: component_hash,
                world: World::Transform1,
                export: "recreate".into(),
            },
            inputs: vec![
                InputRef {
                    hash: layout_hash,
                    role: Some("skeleton".into()),
                },
                InputRef {
                    hash: stripped_hash,
                    role: None,
                },
            ],
            outputs: vec![OutputRef {
                hash: item.hash,
                size,
                name: None,
            }],
            params: Vec::new(),
        };
        crate::mint_recipe(store, db, &recipe, SeekClass::ManifestSeekable)
            .map_err(|e| e.to_string())?;

        Ok(AnalysisResult {
            outcome: AnalysisOutcome::Positive,
            detail: Some(format!(
                "regenerable: {regenerable} sector(s) (m1 {}, m2f1 {}, m2f2 {}), {} literal byte(s); stripped {} B + layout {} B replace {size} B",
                reader.sectors[1],
                reader.sectors[2],
                reader.sectors[3],
                reader.sectors[0],
                aliases.size,
                layout.len()
            )),
        })
    }
}

/// NDS NitroFS decomposition (D83): an NTR ROM is a pure concatenation,
/// so the whole lane is builtins — no component, no sandbox, every
/// recipe `assemble@1` and affine (the D63 carve-out serves rebuilt
/// ROMs, members, and trimmed views without materializing). Per ROM:
///
/// - each piece (binaries, tables, every NitroFS file, residual gaps)
///   becomes a CLAIM (absent blob + ROM→piece slice recipe) — pieces
///   dedupe across regional variants at the resource boundary, which is
///   the whole point (dedupe ladder rank 4);
/// - one rebuild recipe reassembles the ROM from the pieces in PHYSICAL
///   order (header inlined, pad runs as Fill — rank 5), making the
///   literal evictable once pieces are materialized;
/// - when the trim rules validate, the trimmed view's identity is
///   claimed with a FULL alias tuple (trimmed dumps circulate — dat
///   aliases must hit it) plus its prefix-slice recipe.
///
/// Parse refusals are conclusions (D81 Negative, settled); a wrong
/// coverage map can only waste a mint — replay verification (D4) is
/// what bit-faithfulness rests on, never this parser.
pub struct NdsAnalyzer;

impl NdsAnalyzer {
    const VERSIONED_NAME: &'static str = "nds-split/1";
}

impl Analyzer for NdsAnalyzer {
    fn name(&self) -> &'static str {
        Self::VERSIONED_NAME
    }

    fn family(&self) -> &'static str {
        "nds"
    }

    fn id(&self) -> Blake3 {
        analyzer_tag(Self::VERSIONED_NAME)
    }

    fn analyze(
        &mut self,
        item: &SweepItem,
        bytes: &Logical<'_, '_>,
        store: &Store,
        db: &mut Db,
        pulse: &mut dyn Pulse,
    ) -> Result<AnalysisResult, String> {
        let file = bytes.open(item, db, pulse)?;
        let mut rom = TickRandom { inner: file, pulse };

        let layout = match crate::nds::parse_layout(&mut rom) {
            Ok(layout) => layout,
            Err(crate::nds::NdsError::Refused(refusal)) => {
                return Ok(AnalysisResult {
                    outcome: AnalysisOutcome::Negative,
                    detail: Some(refusal.to_string()),
                });
            }
            Err(crate::nds::NdsError::Io(e)) => return Err(format!("reading rom: {e}")),
        };

        // Claim every piece + mint the coverage-map rebuild (shared with
        // the NARC lane one level down — same decomposition arithmetic).
        mint_decomposition(
            store,
            db,
            item.hash,
            layout.rom_len,
            &layout.pieces,
            &layout.regions,
            layout.empty_files,
            &mut rom,
        )?;

        // The trimmed view's identity: a prefix slice, claimed with the
        // full alias tuple so trimmed dumps in the wild dat-match it.
        if let Some(trim_len) = layout.trim_len {
            let tuple = alias_range(&mut rom, trim_len).map_err(|e| e.to_string())?;
            let id = match db.blob_by_hash(&tuple.blake3).map_err(|e| e.to_string())? {
                Some(row) => row.blob_id,
                None => db
                    .upsert_blob(
                        &tuple.blake3,
                        Some(trim_len),
                        IndexNs::Data,
                        Residency::Absent,
                    )
                    .map_err(|e| e.to_string())?,
            };
            db.insert_aliases(id, &tuple).map_err(|e| e.to_string())?;
            let trim = Recipe {
                op: Op::Builtin {
                    name: "assemble".into(),
                    major: 1,
                },
                inputs: vec![InputRef {
                    hash: item.hash,
                    role: Some("nds:trim".into()),
                }],
                outputs: vec![OutputRef {
                    hash: tuple.blake3,
                    size: trim_len,
                    name: None,
                }],
                params: AssembleParams {
                    segments: vec![Segment::BlobRange {
                        input_ix: 0,
                        offset: 0,
                        len: trim_len,
                    }],
                }
                .encode()
                .map_err(|e| e.to_string())?,
            };
            crate::mint_recipe(store, db, &trim, SeekClass::Affine).map_err(|e| e.to_string())?;
        }

        Ok(AnalysisResult {
            outcome: AnalysisOutcome::Positive,
            detail: Some(format!(
                "split into {} piece(s) ({} nitrofs file(s), {} residual byte(s)); trim {}",
                layout.pieces.len(),
                layout.file_count,
                layout.residual_bytes,
                layout.trim_len.map_or_else(
                    || "not offered".to_owned(),
                    |t| format!("{t} of {} bytes", layout.rom_len)
                ),
            )),
        })
    }
}

/// NARC interior decomposition (decomposition-arc step 3): the same
/// affine byte-slicing the nds analyzer does on the ROM container, one
/// level down on a Nitro archive. It runs over any blob that sniffs as
/// a NARC — in practice the NitroFS-file pieces the nds analyzer already
/// claimed (read through the executor, D92) — so a NARC's members dedupe
/// across regional variants at the archive-member boundary, the sharing
/// the whole-NARC blob hides. No wasm: NARC is pure concatenation, every
/// recipe a builtin.
///
/// Recipe-volume gated (`narc:max-members`): a NARC can hold thousands
/// of tiny files, and past a point the claim + recipe volume outweighs
/// the dedup — those stay literals (the whole-archive blob still
/// dedupes, and CDC can still chew it). The interior LZ codecs (SDAT
/// audio, LZ overlays) are a separate, wasm-shaped lane and are NOT
/// attempted here.
pub struct NarcAnalyzer;

impl NarcAnalyzer {
    const VERSIONED_NAME: &'static str = "narc-split/1";
    const DEFAULT_MAX_MEMBERS: usize = 4096;
}

impl Analyzer for NarcAnalyzer {
    fn name(&self) -> &'static str {
        Self::VERSIONED_NAME
    }

    fn family(&self) -> &'static str {
        "narc"
    }

    fn id(&self) -> Blake3 {
        analyzer_tag(Self::VERSIONED_NAME)
    }

    fn analyze(
        &mut self,
        item: &SweepItem,
        bytes: &Logical<'_, '_>,
        store: &Store,
        db: &mut Db,
        pulse: &mut dyn Pulse,
    ) -> Result<AnalysisResult, String> {
        let file = bytes.open(item, db, pulse)?;
        let mut narc = TickRandom { inner: file, pulse };

        let layout = match crate::narc::parse_layout(&mut narc) {
            Ok(layout) => layout,
            Err(crate::nds::NdsError::Refused(refusal)) => {
                return Ok(AnalysisResult {
                    outcome: AnalysisOutcome::Negative,
                    detail: Some(refusal.to_string()),
                });
            }
            Err(crate::nds::NdsError::Io(e)) => return Err(format!("reading narc: {e}")),
        };

        // Recipe-volume gate (D91-style molten policy): a huge-member
        // NARC would flood claims and recipes for marginal dedup.
        let max = db
            .config_get("narc:max-members")
            .map_err(|e| e.to_string())?
            .and_then(|v| std::str::from_utf8(&v).ok()?.trim().parse::<usize>().ok())
            .unwrap_or(Self::DEFAULT_MAX_MEMBERS);
        if layout.file_count > max {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some(format!(
                    "{} members exceed the narc:max-members cap ({max})",
                    layout.file_count
                )),
            });
        }

        mint_decomposition(
            store,
            db,
            item.hash,
            layout.narc_len,
            &layout.pieces,
            &layout.regions,
            layout.empty_files,
            &mut narc,
        )?;

        Ok(AnalysisResult {
            outcome: AnalysisOutcome::Positive,
            detail: Some(format!(
                "split into {} piece(s) ({} member(s), {} residual byte(s))",
                layout.pieces.len(),
                layout.file_count,
                layout.residual_bytes
            )),
        })
    }
}

/// Mint a container decomposition (D83): an absent claim + a
/// `container→piece` slice recipe per piece, then the coverage-map
/// rebuild recipe over the regions. Shared by the .nds container and
/// the NARC interior one level down — the same affine byte-range
/// arithmetic, so one tested mint path serves both. `container` is the
/// blob being decomposed, `total_len` its length, `pieces`/`regions`
/// its exact coverage map (concatenating to `[0, total_len)`).
#[allow(clippy::too_many_arguments)] // container + map + rom, all load-bearing
fn mint_decomposition<R: std::io::Read + std::io::Seek>(
    store: &Store,
    db: &mut Db,
    container: Blake3,
    total_len: u64,
    pieces: &[crate::nds::Piece],
    regions: &[crate::nds::Region],
    empty_files: usize,
    rom: &mut R,
) -> Result<(), String> {
    // Claim every piece: hash its range out of the container, then an
    // absent row (never blind-upserted — a piece already resident from an
    // earlier ingest, the dedupe hit, must not be demoted) + the slice.
    let mut piece_hashes: Vec<Blake3> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let hash = hash_range(rom, piece.start, piece.len).map_err(|e| e.to_string())?;
        if db.blob_by_hash(&hash).map_err(|e| e.to_string())?.is_none() {
            db.upsert_blob(&hash, Some(piece.len), IndexNs::Data, Residency::Absent)
                .map_err(|e| e.to_string())?;
        }
        let recipe = Recipe {
            op: Op::Builtin {
                name: "assemble".into(),
                major: 1,
            },
            inputs: vec![InputRef {
                hash: container,
                role: None,
            }],
            outputs: vec![OutputRef {
                hash,
                size: piece.len,
                name: Some(piece.name.clone()),
            }],
            params: AssembleParams {
                segments: vec![Segment::BlobRange {
                    input_ix: 0,
                    offset: piece.start,
                    len: piece.len,
                }],
            }
            .encode()
            .map_err(|e| e.to_string())?,
        };
        crate::mint_recipe(store, db, &recipe, SeekClass::Affine).map_err(|e| e.to_string())?;
        piece_hashes.push(hash);
    }

    // Zero-length entries: identity is the empty blob — ground it
    // directly (assemble rejects empty outputs, the zip precedent).
    if empty_files > 0 {
        let empty = Blake3::compute(b"");
        store
            .put(StoreNs::Data, empty, std::io::empty())
            .map_err(|e| e.to_string())?;
        db.upsert_blob(&empty, Some(0), IndexNs::Data, Residency::Resident)
            .map_err(|e| e.to_string())?;
    }

    // The rebuild: the coverage map verbatim, one segment per region,
    // inputs deduped by hash (identical pieces at different offsets are
    // one blob).
    let mut inputs: Vec<InputRef> = Vec::new();
    let mut input_ix: std::collections::HashMap<Blake3, u32> = std::collections::HashMap::new();
    let mut segments: Vec<Segment> = Vec::with_capacity(regions.len());
    for region in regions {
        segments.push(match region {
            crate::nds::Region::Piece(ix) => {
                let hash = piece_hashes[*ix];
                let input_ix = *input_ix.entry(hash).or_insert_with(|| {
                    inputs.push(InputRef { hash, role: None });
                    u32::try_from(inputs.len() - 1).expect("piece count fits u32")
                });
                Segment::BlobRange {
                    input_ix,
                    offset: 0,
                    len: pieces[*ix].len,
                }
            }
            crate::nds::Region::Fill { byte, len } => Segment::Fill {
                byte: *byte,
                len: *len,
            },
            crate::nds::Region::Literal { start, len } => Segment::Literal {
                bytes: read_range(rom, *start, *len).map_err(|e| e.to_string())?,
            },
        });
    }
    let rebuild = Recipe {
        op: Op::Builtin {
            name: "assemble".into(),
            major: 1,
        },
        inputs,
        outputs: vec![OutputRef {
            hash: container,
            size: total_len,
            name: None,
        }],
        params: AssembleParams { segments }
            .encode()
            .map_err(|e| e.to_string())?,
    };
    crate::mint_recipe(store, db, &rebuild, SeekClass::Affine).map_err(|e| e.to_string())?;
    Ok(())
}

/// `Read + Seek` over the stored blob that pulses per read — piece
/// hashing and gap classification are the long loops here, and bytes
/// moving is the lease heartbeat (D71).
struct TickRandom<'p> {
    inner: datboi_store_fs::Blob,
    pulse: &'p mut dyn Pulse,
}

impl std::io::Read for TickRandom<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.pulse.tick(n as u64);
        }
        Ok(n)
    }
}

impl std::io::Seek for TickRandom<'_> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// blake3 of `[start, start+len)` of a seekable source, streamed.
fn hash_range<R: std::io::Read + std::io::Seek>(
    rom: &mut R,
    start: u64,
    len: u64,
) -> std::io::Result<Blake3> {
    rom.seek(std::io::SeekFrom::Start(start))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut remaining = len;
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buf.len() as u64)).expect("bounded");
        rom.read_exact(&mut buf[..want])?;
        hasher.update(&buf[..want]);
        remaining -= want as u64;
    }
    Ok(Blake3(*hasher.finalize().as_bytes()))
}

/// Full alias tuple of the `[0, len)` prefix, streamed.
fn alias_range<R: std::io::Read + std::io::Seek>(
    rom: &mut R,
    len: u64,
) -> std::io::Result<datboi_core::alias::AliasTuple> {
    rom.seek(std::io::SeekFrom::Start(0))?;
    let mut hasher = datboi_core::alias::AliasHasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut remaining = len;
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buf.len() as u64)).expect("bounded");
        rom.read_exact(&mut buf[..want])?;
        hasher.update(&buf[..want]);
        remaining -= want as u64;
    }
    Ok(hasher.finalize())
}

/// Raw bytes of `[start, start+len)` — inline-literal segments only
/// (bounded by assemble's cap).
fn read_range<R: std::io::Read + std::io::Seek>(
    rom: &mut R,
    start: u64,
    len: u64,
) -> std::io::Result<Vec<u8>> {
    rom.seek(std::io::SeekFrom::Start(start))?;
    let mut buf = vec![0u8; usize::try_from(len).expect("literal segments are capped")];
    rom.read_exact(&mut buf)?;
    Ok(buf)
}
