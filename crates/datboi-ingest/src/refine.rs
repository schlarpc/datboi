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

    /// Stable operator-facing family name — the D60 config key. Unlike
    /// [`Analyzer::name`]/[`Analyzer::id`], it survives version bumps:
    /// an operator's enable/disable choice carries across analyzer
    /// revisions (identity re-runs the fixpoint; policy stays).
    fn family(&self) -> &'static str;

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

    fn family(&self) -> &'static str {
        "noop"
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
    /// The analyzer family is disabled (D60): nothing ran.
    pub disabled: bool,
}

// ---- D60 ingest-policy config: the minimal shape ----
//
// Per-analyzer enable/disable + analyzer-owned opaque params in the
// state.db config KV (`analyzer:<family>:enabled` / `:params`), which
// rides the state snapshot like every other config row. Sweep ordering
// stays a single global dat-aware policy — no per-analyzer knobs.

fn enabled_key(family: &str) -> String {
    format!("analyzer:{family}:enabled")
}

fn params_key(family: &str) -> String {
    format!("analyzer:{family}:params")
}

/// Is the family enabled? Absent means yes (opt-out policy).
///
/// # Errors
/// Index I/O.
pub fn analyzer_enabled(db: &Db, family: &str) -> Result<bool, datboi_index::IndexError> {
    Ok(db
        .config_get(&enabled_key(family))?
        .is_none_or(|v| v != b"0"))
}

/// # Errors
/// Index I/O.
pub fn set_analyzer_enabled(
    db: &Db,
    family: &str,
    enabled: bool,
) -> Result<(), datboi_index::IndexError> {
    db.config_set(&enabled_key(family), if enabled { b"1" } else { b"0" })
}

/// Analyzer-owned opaque params (the analyzer defines the encoding;
/// nothing else interprets them). Empty and absent are both `None`.
///
/// # Errors
/// Index I/O.
pub fn analyzer_params(db: &Db, family: &str) -> Result<Option<Vec<u8>>, datboi_index::IndexError> {
    Ok(db
        .config_get(&params_key(family))?
        .filter(|v| !v.is_empty()))
}

/// Set (or clear, with `None`) a family's opaque params.
///
/// # Errors
/// Index I/O.
pub fn set_analyzer_params(
    db: &Db,
    family: &str,
    params: Option<&[u8]>,
) -> Result<(), datboi_index::IndexError> {
    db.config_set(&params_key(family), params.unwrap_or(b""))
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
    // D60: policy gate at the one entry point every sweep caller uses.
    if !analyzer_enabled(db, analyzer.family())? {
        return Ok(SweepReport {
            disabled: true,
            ..SweepReport::default()
        });
    }
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
