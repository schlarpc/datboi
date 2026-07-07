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

/// Trial-recompression discovery (D24/D45): can each DEFLATE member of a
/// zip container be byte-identically reproduced by OUR deterministic
/// deflate (miniz_oxide, flate2's rust backend) at some compression
/// level? A container whose every member matches is rebuildable from
/// extracted members — the wild-zip shrink prerequisite.
///
/// SKELETON HONESTY: this analyzer only *discovers and records* — it
/// mints no rebuild recipes yet, because a D5-compliant rebuild recipe
/// must pin a wasm compressor component (compression is never builtin —
/// docs/70-recipes.md), and the `xf-deflate` component (same
/// miniz_oxide, compiled for wasm32-unknown-unknown, output
/// byte-identical to this native trial) hasn't shipped. The expensive
/// part — the per-member level search and especially the NEGATIVES —
/// is exactly what D48 wants recorded once, forever. Expect low match
/// rates on scene zips (they're mostly zlib/TorrentZip output, which
/// miniz does not reproduce); a zlib-exact component is the flagged
/// follow-up.
pub struct DeflateTrialAnalyzer;

impl DeflateTrialAnalyzer {
    const VERSIONED_NAME: &'static str = "deflate-trial-miniz/1";

    /// Levels worth trying, most common first (TorrentZip is 9; most
    /// tools default to 6).
    const LEVELS: [u32; 4] = [9, 6, 1, 5];
}

impl Analyzer for DeflateTrialAnalyzer {
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
        _db: &mut Db,
    ) -> Result<AnalysisResult, String> {
        use std::io::{Read, Seek, SeekFrom};

        let Some(mut file) = store
            .get(StoreNs::Data, &item.hash)
            .map_err(|e| e.to_string())?
        else {
            return Err("blob not resident".into());
        };
        let mut head = [0u8; 4];
        let n = file.read(&mut head).map_err(|e| e.to_string())?;
        if !crate::zip::looks_like_zip(&head[..n]) {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some("not a zip container".into()),
            });
        }
        let parsed = crate::zip::parse_members(&mut file).map_err(|e| e.to_string())?;
        let mut deflate_members = 0usize;
        let mut matched = 0usize;
        let mut levels: Vec<String> = Vec::new();
        for member in &parsed.members {
            if member.method != crate::zip::Method::Deflate {
                continue;
            }
            deflate_members += 1;
            // Read the member's compressed bytes once (bounded by the
            // container's own size; wild zips with multi-GB members get
            // a streaming trial when the wasm component lands).
            file.seek(SeekFrom::Start(member.data_start))
                .map_err(|e| e.to_string())?;
            let mut compressed =
                vec![0u8; usize::try_from(member.comp_size).map_err(|e| e.to_string())?];
            file.read_exact(&mut compressed)
                .map_err(|e| e.to_string())?;
            let mut plain = Vec::new();
            flate2::read::DeflateDecoder::new(compressed.as_slice())
                .read_to_end(&mut plain)
                .map_err(|e| format!("member does not inflate: {e}"))?;
            let hit = Self::LEVELS.iter().find(|level| {
                let mut enc = flate2::write::DeflateEncoder::new(
                    Vec::new(),
                    flate2::Compression::new(**level),
                );
                std::io::Write::write_all(&mut enc, &plain).expect("vec write");
                enc.finish().expect("vec finish") == compressed
            });
            if let Some(level) = hit {
                matched += 1;
                levels.push(format!("{}@{level}", member.name));
            }
        }
        if deflate_members == 0 {
            return Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some("no deflate members to reproduce".into()),
            });
        }
        if matched == deflate_members {
            Ok(AnalysisResult {
                outcome: AnalysisOutcome::Positive,
                detail: Some(format!(
                    "rebuildable: all {deflate_members} deflate member(s) reproduce ({})",
                    levels.join(", ")
                )),
            })
        } else {
            Ok(AnalysisResult {
                outcome: AnalysisOutcome::Negative,
                detail: Some(format!(
                    "{matched}/{deflate_members} deflate member(s) reproduce; container stays literal (D24)"
                )),
            })
        }
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
