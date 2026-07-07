//! The refinement fixpoint skeleton (D45/D47/D48): background sweeps that
//! advance analysis over the corpus. Ingest is custody; everything
//! expensive happens here, later, resumably.
//!
//! An analyzer is a pure function of `bytes × analyzer identity`. Its
//! identity hash pins exactly what ran: a wasm analyzer's component hash,
//! or [`analyzer_tag`] over a versioned name for native ones. Shipping a
//! new analyzer version = new identity = every blob becomes unanalyzed
//! again for it — "new analyzer ships" and "keys arrive late" are the
//! same event, and the fixpoint advances (D45).
//!
//! The sweep loop is crash-safe by at-least-once: provenance row +
//! queue-row removal commit together AFTER the work; a crash mid-item
//! re-runs a pure function.

use std::time::{SystemTime, UNIX_EPOCH};

use datboi_core::hash::Blake3;
use datboi_index::{AnalysisOutcome, Db, SweepItem};
use datboi_store_fs::Store;

/// Identity hash for a native (non-wasm) analyzer: a domain-separated
/// hash over a versioned name, e.g. `"noop/1"`. Bump the version to make
/// the fixpoint re-run it over everything.
#[must_use]
pub fn analyzer_tag(versioned_name: &str) -> Blake3 {
    Blake3::compute(format!("datboi-analyzer:{versioned_name}").as_bytes())
}

/// What one analysis produced.
pub struct AnalysisResult {
    pub outcome: AnalysisOutcome,
    /// Analyzer-owned annotation (why negative / what was minted).
    pub detail: Option<String>,
}

/// One analyzer. Implementations mint recipes/claims through their own
/// side effects (they get store + db access) and report provenance via
/// the returned result. What they claim must be dat-blind (D47).
pub trait Analyzer {
    /// Versioned, human-readable name (also the CLI selector).
    fn name(&self) -> &'static str;

    /// Identity hash — what provenance rows pin (D48).
    fn id(&self) -> Blake3;

    /// Analyze one blob's bytes.
    ///
    /// # Errors
    /// A per-blob error string: recorded nowhere, item stays queued (the
    /// environment failed, not the analysis — a negative CONCLUSION must
    /// be returned as `Ok(Negative)`).
    fn analyze(
        &mut self,
        item: &SweepItem,
        store: &Store,
        db: &mut Db,
    ) -> Result<AnalysisResult, String>;
}

/// A sweep that concludes "nothing found" about everything it touches —
/// the fixpoint plumbing proof (D50 exit criterion: a no-op analyzer
/// sweep records provenance and survives the recovery drill).
pub struct NoopAnalyzer;

impl Analyzer for NoopAnalyzer {
    fn name(&self) -> &'static str {
        "noop/1"
    }

    fn id(&self) -> Blake3 {
        analyzer_tag(self.name())
    }

    fn analyze(
        &mut self,
        _item: &SweepItem,
        _store: &Store,
        _db: &mut Db,
    ) -> Result<AnalysisResult, String> {
        Ok(AnalysisResult {
            outcome: AnalysisOutcome::Negative,
            detail: None,
        })
    }
}

#[derive(Debug, Default)]
pub struct SweepReport {
    pub enqueued: usize,
    pub analyzed: usize,
    pub positive: usize,
    pub negative: usize,
    /// (blob hash, error) — items left queued for a later sweep.
    pub errors: Vec<(Blake3, String)>,
}

/// Run one sweep round: enqueue unanalyzed candidates (dat-blind),
/// apply dat-aware ordering, then process up to `limit` items.
///
/// # Errors
/// Database errors abort the sweep; per-blob analyzer errors are
/// reported and leave their items queued.
pub fn run_sweep(
    db: &mut Db,
    store: &Store,
    analyzer: &mut dyn Analyzer,
    limit: usize,
) -> Result<SweepReport, datboi_index::IndexError> {
    let id = analyzer.id();
    let now = now_unix();
    let mut report = SweepReport {
        enqueued: db.enqueue_unanalyzed(&id, now)?,
        ..SweepReport::default()
    };
    db.bump_dat_matched_priorities()?;

    let items = db.next_sweep_items(&id, limit)?;
    for item in items {
        match analyzer.analyze(&item, store, db) {
            Ok(result) => {
                db.complete_sweep_item(
                    item.blob_id,
                    &id,
                    result.outcome,
                    result.detail.as_deref(),
                    now_unix(),
                )?;
                report.analyzed += 1;
                match result.outcome {
                    AnalysisOutcome::Positive => report.positive += 1,
                    AnalysisOutcome::Negative => report.negative += 1,
                }
            }
            Err(e) => report.errors.push((item.hash, e)),
        }
    }
    Ok(report)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
