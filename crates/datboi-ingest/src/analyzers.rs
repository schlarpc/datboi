//! Real analyzers for the refinement fixpoint (M3, D45). Each one is a
//! pure function of bytes × its identity tag; discoveries are minted as
//! ordinary recipes and the provenance row records why (or why not).

use std::io::BufReader;

use datboi_core::assemble::{AssembleParams, Segment};
use datboi_core::hash::Blake3;
use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe};
use datboi_index::{AnalysisOutcome, Db, Namespace as IndexNs, Residency, SeekClass, SweepItem};
use datboi_store_fs::{Namespace as StoreNs, Store};

use crate::refine::{AnalysisResult, Analyzer, analyzer_tag};

/// FastCDC parameters (D3 strategy ladder, rung 3): gear hash, NC level
/// 2, 64 KiB / 256 KiB / 1 MiB — tuned for disc images. The values are
/// baked into the analyzer identity: retuning mints a NEW analyzer whose
/// sweep re-covers the corpus, while old recipes stay valid forever
/// (chunker identity is provenance, not replay input — docs/70-recipes.md).
pub const CHUNK_MIN: usize = 64 * 1024;
pub const CHUNK_AVG: usize = 256 * 1024;
pub const CHUNK_MAX: usize = 1024 * 1024;

/// Blobs below this aren't worth a chunk recipe (they ARE a chunk).
/// Provisional policy, molten with the rest of the ingest-policy config
/// vocabulary (open-questions.md).
pub const CHUNK_THRESHOLD: u64 = 4 * 1024 * 1024;

/// The canonical `xf-preflate` component (D53), embedded so the analyzer
/// can publish it as an ordinary CAS blob the moment it mints the first
/// recipe that pins it. `transforms/dist/` holds the committed
/// reproducible build (the runtime gate pins the same bytes by blake3).
pub const XF_PREFLATE_WASM: &[u8] = include_bytes!("../../../transforms/dist/xf_preflate.wasm");

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
    src: &'a mut std::fs::File,
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
    fn new(src: &'a mut std::fs::File, comp_size: u64) -> Self {
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
                self.window.resize(start + usize::try_from(take).expect("bounded"), 0);
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
                    let (Ok(pt_len), Ok(corr_len)) = (
                        u32::try_from(pt.len()),
                        u32::try_from(r.corrections.len()),
                    ) else {
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

    fn id(&self) -> Blake3 {
        analyzer_tag(Self::VERSIONED_NAME)
    }

    fn analyze(
        &mut self,
        item: &SweepItem,
        store: &Store,
        db: &mut Db,
    ) -> Result<AnalysisResult, String> {
        use std::io::{Read, Seek, SeekFrom};

        let Some(mut file) = store
            .get(StoreNs::Data, &item.hash)
            .map_err(|e| e.to_string())?
        else {
            return Err("blob not resident".into());
        };
        let container_size = item
            .size
            .or_else(|| store.len(StoreNs::Data, &item.hash).ok().flatten())
            .ok_or("container size unknown")?;
        let mut head = [0u8; 4];
        let n = file.read(&mut head).map_err(|e| e.to_string())?;
        if !crate::zip::looks_like_zip(&head[..n]) {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some("not a zip container".into()),
            });
        }
        let parsed = crate::zip::parse_members(&mut file).map_err(|e| e.to_string())?;
        if !parsed.skipped.is_empty() {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some(format!(
                    "{} member(s) outside the supported subset; container stays literal (D24)",
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
        let mut skeleton_len = 0u64;
        for &(start, len, _) in &ranges {
            if start < prev_end || start.checked_add(len).is_none_or(|e| e > container_size) {
                return Ok(AnalysisResult {
                    outcome: AnalysisOutcome::Negative,
                    detail: Some("overlapping or out-of-bounds member ranges".into()),
                });
            }
            skeleton_len += start - prev_end;
            prev_end = start + len;
        }
        skeleton_len += container_size - prev_end;
        if skeleton_len > SKELETON_LIMIT {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some(format!(
                    "skeleton would be {skeleton_len} bytes (> {SKELETON_LIMIT}); container stays literal"
                )),
            });
        }

        // ---- split every member (all-or-nothing, D24) ----
        let mut splits: Vec<MemberSplit> = Vec::with_capacity(ranges.len());
        for &(start, len, ix) in &ranges {
            file.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
            let mut reader = SplitReader::new(&mut file, len);
            let put = store.put_new(StoreNs::Data, &mut reader);
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
                        .put(StoreNs::Data, corrections_hash, reader.corrections.as_slice())
                        .map_err(|e| e.to_string())?;
                    let stream_hash = Blake3(*reader.stream_hasher.finalize().as_bytes());
                    splits.push(MemberSplit {
                        stream_hash,
                        corrections_hash,
                        corrections_len: reader.corrections.len() as u64,
                        plaintext_hash,
                        plaintext_len: aliases.size,
                    });
                }
                Err(e) => {
                    // Deterministic split refusals are the D48 negative
                    // this analyzer exists to record; real I/O stays an
                    // analyzer error (retryable).
                    if let Some(msg) = reader.fail.take() {
                        return Ok(AnalysisResult {
                            outcome: AnalysisOutcome::Negative,
                            detail: Some(format!(
                                "member {:?}: {msg}; container stays literal (D24)",
                                deflate_members[ix].name
                            )),
                        });
                    }
                    return Err(e.to_string());
                }
            }
        }

        // ---- skeleton: every byte outside member data, in order ----
        let mut skeleton = Vec::with_capacity(usize::try_from(skeleton_len).unwrap_or(0));
        let mut gaps: Vec<(u64, u64)> = Vec::new(); // (container offset, len)
        let mut prev_end = 0u64;
        for &(start, len, _) in &ranges {
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
        let skeleton_id = db
            .upsert_blob(
                &skeleton_hash,
                Some(skeleton.len() as u64),
                IndexNs::Data,
                Residency::Resident,
            )
            .map_err(|e| e.to_string())?;

        // ---- mint: one recreate recipe per member ----
        let component_id = self.ensure_component(store, db)?;
        let component_hash = Self::component_hash();
        let mut stream_ids: Vec<i64> = Vec::with_capacity(splits.len());
        for (&(_, stream_len, _), split) in ranges.iter().zip(&splits) {
            let corrections_id = db
                .upsert_blob(
                    &split.corrections_hash,
                    Some(split.corrections_len),
                    IndexNs::Data,
                    Residency::Resident,
                )
                .map_err(|e| e.to_string())?;
            let plaintext_id = db
                .upsert_blob(
                    &split.plaintext_hash,
                    Some(split.plaintext_len),
                    IndexNs::Data,
                    Residency::Resident,
                )
                .map_err(|e| e.to_string())?;
            // The stream itself: known hash, no local bytes — it lives
            // inside the container until eviction, and the recipe is its
            // route thereafter.
            let stream_id = db
                .upsert_blob(
                    &split.stream_hash,
                    Some(stream_len),
                    IndexNs::Data,
                    Residency::Absent,
                )
                .map_err(|e| e.to_string())?;
            stream_ids.push(stream_id);
            let recipe = Recipe {
                op: Op::Wasm {
                    component: component_hash,
                    world: "datboi:transform@2".into(),
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
            crate::mint_recipe(
                store,
                db,
                &recipe,
                "xf-preflate/recreate",
                SeekClass::Opaque,
                &[
                    (0, corrections_id, Some("skeleton")),
                    (1, plaintext_id, None),
                ],
                &[(0, stream_id, stream_len, None)],
            )
            .map_err(|e| e.to_string())?;
            let _ = component_id; // pinned via the recipe op; row ensured above
        }

        // ---- mint: the container assemble over skeleton + streams ----
        let mut inputs = vec![InputRef {
            hash: skeleton_hash,
            role: Some("skeleton".into()),
        }];
        let mut input_rows: Vec<(u32, i64, Option<&str>)> =
            vec![(0, skeleton_id, Some("skeleton"))];
        for (ix, split) in splits.iter().enumerate() {
            inputs.push(InputRef {
                hash: split.stream_hash,
                role: None,
            });
            input_rows.push((
                u32::try_from(ix + 1).expect("member count fits u32"),
                stream_ids[ix],
                None,
            ));
        }
        let mut segments: Vec<Segment> = Vec::new();
        let mut skel_off = 0u64;
        let mut prev_end = 0u64;
        for (range_ix, &(start, len, _)) in ranges.iter().enumerate() {
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
        crate::mint_recipe(
            store,
            db,
            &recipe,
            "assemble@1",
            SeekClass::Affine,
            &input_rows,
            &[(0, item.blob_id, container_size, None)],
        )
        .map_err(|e| e.to_string())?;

        let corr_total: u64 = splits.iter().map(|s| s.corrections_len).sum();
        let pt_total: u64 = splits.iter().map(|s| s.plaintext_len).sum();
        Ok(AnalysisResult {
            outcome: AnalysisOutcome::Positive,
            detail: Some(format!(
                "rebuildable: {} member(s) split; plaintext {pt_total} B, corrections {corr_total} B, skeleton {} B",
                splits.len(),
                skeleton.len()
            )),
        })
    }
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

    fn id(&self) -> Blake3 {
        analyzer_tag(Self::VERSIONED_NAME)
    }

    fn analyze(
        &mut self,
        item: &SweepItem,
        store: &Store,
        db: &mut Db,
    ) -> Result<AnalysisResult, String> {
        let Some(size) = item
            .size
            .or_else(|| store.len(StoreNs::Data, &item.hash).ok().flatten())
        else {
            return Err("blob absent from store".into());
        };
        if size < CHUNK_THRESHOLD {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some(format!("below {CHUNK_THRESHOLD}-byte chunking threshold")),
            });
        }
        let Some(file) = store
            .get(StoreNs::Data, &item.hash)
            .map_err(|e| e.to_string())?
        else {
            // Evicted or absent: nothing to chunk right now; the item
            // stays queued for a sweep after rematerialization.
            return Err("blob not resident".into());
        };

        // One streaming pass: chunk, store each chunk, build segments.
        let mut inputs: Vec<InputRef> = Vec::new();
        let mut input_rows: Vec<(u32, i64, Option<&str>)> = Vec::new();
        let mut segments: Vec<Segment> = Vec::new();
        let mut chunk_ids: Vec<(Blake3, u64)> = Vec::new();
        let chunker = fastcdc::v2020::StreamCDC::with_level(
            BufReader::new(file),
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
            let blob_id = db
                .upsert_blob(hash, Some(*len), IndexNs::Data, Residency::Resident)
                .map_err(|e| e.to_string())?;
            inputs.push(InputRef {
                hash: *hash,
                role: None,
            });
            input_rows.push((
                u32::try_from(ix).expect("chunk count fits u32"),
                blob_id,
                None,
            ));
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
        crate::mint_recipe(
            store,
            db,
            &recipe,
            "assemble@1",
            SeekClass::Affine,
            &input_rows,
            &[(0, item.blob_id, size, None)],
        )
        .map_err(|e| e.to_string())?;

        Ok(AnalysisResult {
            outcome: AnalysisOutcome::Positive,
            detail: Some(format!("chunked into {} pieces", chunk_ids.len())),
        })
    }
}
