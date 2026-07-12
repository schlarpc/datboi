//! Command implementations (docs/85-cli.md). Human tables by default,
//! `--json` everywhere; audit exit codes: 0 complete / 1 incomplete /
//! 2 error.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use datboi_catalog::{ImportOptions, audit, diff_source, export_dat, import_dat};
use datboi_core::alias::AliasHasher;
use datboi_core::object::{self, ObjectKind};
use datboi_core::recipe::{Op, Recipe};
use datboi_core::snapshot::{
    AliasBatch, AnalysisBatch, SnapshotPayload, SourceRef, StateSnapshot, alias_shard,
};
use datboi_index::recipes::NewRecipe;
use datboi_index::types::{Namespace as NsRow, OpKind, RecipeSource, Residency, SeekClass};
use datboi_ingest::{IngestReport, Ingester};
use datboi_store_fs::{Namespace, StoreError, VerifyOutcome};
use serde_json::json;

use crate::config::Env;

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Split "provider/system" (system may itself contain slashes).
fn split_source(arg: &str) -> anyhow::Result<(&str, &str)> {
    arg.split_once('/')
        .filter(|(p, s)| !p.is_empty() && !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("expected <provider>/<system>, got {arg:?}"))
}

fn warn_detector_errors(env: &Env) {
    for (path, err) in &env.detector_errors {
        eprintln!("warning: detector {}: {err}", path.display());
    }
}

// ---- ingest ----

pub fn ingest(mut env: Env, paths: &[PathBuf], mv: bool, json: bool) -> anyhow::Result<ExitCode> {
    if mv {
        // D40 custody semantics (delete source only after index rows are
        // durable) need a per-file hook the Ingester doesn't expose yet;
        // shipping half of --move would be worse than none.
        bail!("--move is not implemented yet; ingest defaults to --copy semantics (D40)");
    }
    warn_detector_errors(&env);
    // Best-effort sweep of crash-orphaned temp files (docs/10-cas.md).
    if let Ok(swept) = env.store.cleanup_temp(Duration::from_secs(24 * 60 * 60))
        && swept > 0
    {
        eprintln!("note: removed {swept} stale temp file(s)");
    }
    let detectors = std::mem::take(&mut env.detectors);
    let report = Ingester::new(&env.store, &mut env.db, &detectors).ingest(paths);
    // The pipeline stores content; identity linking + the D39 rollup
    // refresh are what make audit/status see it. Ingest owns finishing
    // that thought (dat import and view eval already run the same pair)
    // instead of leaving new blobs dark until an unrelated eval.
    datboi_catalog::relink_all(&env.db)?;
    datboi_catalog::refresh_rollups(&mut env.db, now_unix())?;
    if json {
        println!("{}", ingest_json(&report));
    } else {
        print_ingest(&report);
    }
    Ok(if report.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn ingest_json(r: &IngestReport) -> serde_json::Value {
    json!({
        "files_scanned": r.files_scanned,
        "files_unchanged": r.files_unchanged,
        "files_stored": r.files_stored,
        "files_already_present": r.files_already_present,
        "members_claimed": r.members_claimed,
        "detector_hits": r.detector_hits,
        "skipper_skipped_large": r.skipper_skipped_large,
        "errors": r.errors.iter()
            .map(|(p, e)| json!({"path": p.display().to_string(), "error": e}))
            .collect::<Vec<_>>(),
        "member_skips": r.member_skips.iter()
            .map(|(p, m, e)| json!({"path": p.display().to_string(), "member": m, "reason": e}))
            .collect::<Vec<_>>(),
        "notes": r.notes,
    })
}

fn print_ingest(r: &IngestReport) {
    println!("scanned            {:>8}", r.files_scanned);
    println!("unchanged (cache)  {:>8}", r.files_unchanged);
    println!("stored             {:>8}", r.files_stored);
    println!("already present    {:>8}", r.files_already_present);
    println!("members claimed    {:>8}", r.members_claimed);
    println!("members extracted  {:>8}", r.members_extracted);
    println!("detector hits      {:>8}", r.detector_hits);
    if r.skipper_skipped_large > 0 {
        println!(
            "skipper skipped    {:>8} (over size cap)",
            r.skipper_skipped_large
        );
    }
    for note in &r.notes {
        println!("note: {note}");
    }
    for (path, member, reason) in &r.member_skips {
        println!("skip: {} :: {member}: {reason}", path.display());
    }
    for (path, err) in &r.errors {
        println!("error: {}: {err}", path.display());
    }
}

// ---- dat import / list ----

pub fn dat_import(
    mut env: Env,
    file: &Path,
    provider: Option<&str>,
    system: Option<&str>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let bytes = std::fs::read(file).with_context(|| format!("reading dat {}", file.display()))?;
    let report = import_dat(
        &env.store,
        &mut env.db,
        &bytes,
        &ImportOptions {
            provider,
            system,
            imported_at: now_unix(),
        },
    )?;
    if json {
        println!(
            "{}",
            json!({
                "source_id": report.source_id,
                "revision_id": report.revision_id,
                "dat_blob": report.dat_blob.to_hex(),
                "entries": report.entries,
                "claims": report.claims,
                "demoted_revisions": report.demoted_revisions,
            })
        );
    } else {
        println!(
            "imported revision {} ({} entries, {} claims) as blob {}",
            report.revision_id, report.entries, report.claims, report.dat_blob
        );
        if !report.demoted_revisions.is_empty() {
            println!(
                "demoted to header-only (D38): revisions {:?}",
                report.demoted_revisions
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn dat_list(env: &Env, json: bool) -> anyhow::Result<ExitCode> {
    let conn = env.db.cache();
    let mut stmt = conn.prepare(
        "SELECT s.provider, s.system, r.revision_id, r.version, r.dat_date, r.imported_at,
                (SELECT COUNT(*) FROM entry e WHERE e.revision_id = r.revision_id)
         FROM dat_source s
         LEFT JOIN dat_revision r ON r.revision_id = s.current_revision_id
         ORDER BY s.provider, s.system",
    )?;
    type SourceRow = (
        String,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<i64>,
        i64,
    );
    let rows: Vec<SourceRow> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    if json {
        let items: Vec<_> = rows
            .iter()
            .map(|(p, s, rev, ver, date, at, entries)| {
                json!({
                    "provider": p, "system": s, "current_revision": rev,
                    "version": ver, "date": date, "imported_at": at,
                    "entries": entries,
                })
            })
            .collect();
        println!("{}", json!({ "sources": items }));
    } else if rows.is_empty() {
        println!("no dat sources imported");
    } else {
        for (p, s, rev, ver, date, _at, entries) in &rows {
            println!(
                "{p}/{s}  rev {}  version {}  date {}  entries {entries}",
                rev.map_or_else(|| "-".into(), |r| r.to_string()),
                ver.as_deref().unwrap_or("-"),
                date.as_deref().unwrap_or("-"),
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---- audit ----

pub fn audit_cmd(
    env: &Env,
    source: &str,
    missing_only: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let (provider, system) = split_source(source)?;
    let report = audit(&env.db, provider, system)?;
    let t = &report.totals;
    let complete = t.missing == 0 && t.probable == 0;
    if json {
        let entries: Vec<_> = report
            .entries
            .iter()
            .filter(|e| !missing_only || e.missing > 0)
            .map(|e| {
                json!({
                    "name": e.name, "required": e.required,
                    "have_verified": e.have_verified, "have_claimed": e.have_claimed,
                    "probable": e.probable, "peer_available": e.peer_available,
                    "missing": e.missing, "mia": e.mia,
                    "complete": e.complete(),
                })
            })
            .collect();
        println!(
            "{}",
            json!({
                "provider": report.provider, "system": report.system,
                "revision_id": report.revision_id,
                "totals": {
                    "entries": t.entries, "entries_complete": t.entries_complete,
                    "required": t.required, "have_verified": t.have_verified,
                    "have_claimed": t.have_claimed, "probable": t.probable,
                    "peer_available": t.peer_available, "missing": t.missing,
                    "mia": t.mia,
                },
                "complete": complete,
                "entries": entries,
            })
        );
    } else {
        println!(
            "{}/{} (revision {})",
            report.provider, report.system, report.revision_id
        );
        println!(
            "entries {}/{} complete; roms: {} verified, {} claimed, {} probable, {} peer, {} missing ({} mia)",
            t.entries_complete,
            t.entries,
            t.have_verified,
            t.have_claimed,
            t.probable,
            t.peer_available,
            t.missing,
            t.mia,
        );
        for e in &report.entries {
            if e.complete() && missing_only {
                continue;
            }
            if e.complete() && !missing_only {
                continue; // tables list problems; --json lists everything
            }
            println!(
                "  {}  missing {}/{}{}",
                e.name,
                e.missing,
                e.required,
                if e.probable > 0 {
                    format!(" (+{} probable)", e.probable)
                } else {
                    String::new()
                }
            );
        }
    }
    Ok(if complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

// ---- export ----

pub fn export_dat_cmd(env: &Env, source: &str, out: &Path) -> anyhow::Result<ExitCode> {
    let (provider, system) = split_source(source)?;
    let bytes = export_dat(&env.db, provider, system, None)?;
    std::fs::write(out, &bytes).with_context(|| format!("writing {}", out.display()))?;
    println!("wrote {} ({} bytes)", out.display(), bytes.len());
    Ok(ExitCode::SUCCESS)
}

// ---- dat fetch ----

/// Resolve a fetch source: a full URL passes through; `redump/<slug>`
/// expands to the stable datfile endpoint (D16: Redump auto-fetches
/// normally; No-Intro stays a manual drop).
fn fetch_url(source: &str) -> anyhow::Result<(String, Option<&str>)> {
    if source.starts_with("http://") || source.starts_with("https://") {
        return Ok((source.to_owned(), None));
    }
    if let Some(slug) = source.strip_prefix("redump/") {
        anyhow::ensure!(
            !slug.is_empty() && slug.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "bad redump system slug {slug:?}"
        );
        return Ok((format!("http://redump.org/datfile/{slug}/"), Some("Redump")));
    }
    bail!("expected a URL or redump/<system-slug>, got {source:?}");
}

/// If `bytes` is a zip, extract its single .dat member; otherwise pass
/// through (Redump serves zips; a direct URL may serve a bare dat).
fn unwrap_fetched_dat(bytes: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    if !datboi_ingest::zip::looks_like_zip(&bytes) {
        return Ok(bytes);
    }
    let mut cursor = std::io::Cursor::new(&bytes);
    let parsed = datboi_ingest::zip::parse_members(&mut cursor)
        .map_err(|e| anyhow::anyhow!("fetched zip: {e}"))?;
    let dats: Vec<_> = parsed
        .members
        .iter()
        .filter(|m| m.name.to_ascii_lowercase().ends_with(".dat"))
        .collect();
    let [member] = dats[..] else {
        bail!(
            "fetched zip has {} .dat members, expected exactly 1",
            dats.len()
        );
    };
    let start = usize::try_from(member.data_start).context("zip offset")?;
    let len = usize::try_from(member.comp_size).context("zip size")?;
    let raw = bytes
        .get(start..start + len)
        .context("zip member out of bounds")?;
    match member.method {
        datboi_ingest::zip::Method::Stored => Ok(raw.to_vec()),
        datboi_ingest::zip::Method::Deflate => {
            let mut out =
                Vec::with_capacity(usize::try_from(member.uncomp_size).unwrap_or(raw.len() * 4));
            flate2::read::DeflateDecoder::new(raw)
                .read_to_end(&mut out)
                .context("inflating fetched dat")?;
            Ok(out)
        }
    }
}

/// Fetch a dat over HTTP and run it through the normal import path (the
/// artifact enters CAS first; import stays a deterministic function of the
/// CAS blob, D15). One polite request: honest User-Agent, 60s timeout,
/// no retries — a failed fetch degrades to a manual drop (D16).
pub fn dat_fetch(
    mut env: Env,
    source: &str,
    provider: Option<&str>,
    system: Option<&str>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let (url, provider_default) = fetch_url(source)?;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("datboi/", env!("CARGO_PKG_VERSION")))
        .build();
    let response = agent
        .get(&url)
        .call()
        .with_context(|| format!("fetching {url}"))?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(256 << 20) // a dat is never this big; a hostile server might be
        .read_to_end(&mut body)
        .context("reading fetch body")?;
    let bytes = unwrap_fetched_dat(body)?;

    let report = import_dat(
        &env.store,
        &mut env.db,
        &bytes,
        &ImportOptions {
            provider: provider.or(provider_default),
            system,
            imported_at: now_unix(),
        },
    )?;
    if json {
        println!(
            "{}",
            json!({
                "url": url,
                "source_id": report.source_id,
                "revision_id": report.revision_id,
                "dat_blob": report.dat_blob.to_hex(),
                "entries": report.entries,
                "claims": report.claims,
            })
        );
    } else {
        println!(
            "fetched {url} -> revision {} ({} entries, {} claims) as blob {}",
            report.revision_id, report.entries, report.claims, report.dat_blob
        );
    }
    Ok(ExitCode::SUCCESS)
}

// ---- dat diff ----

/// Diff previous → current revision (D38 keeps exactly those two
/// materialized). Exit code mirrors diff(1): 0 identical, 1 different.
pub fn dat_diff(env: &Env, source: &str, json: bool) -> anyhow::Result<ExitCode> {
    let (provider, system) = split_source(source)?;
    let diff = diff_source(&env.db, provider, system)?;
    if json {
        println!(
            "{}",
            json!({
                "provider": diff.provider,
                "system": diff.system,
                "revision_old": diff.revision_old,
                "revision_new": diff.revision_new,
                "entries_old": diff.entries_old,
                "entries_new": diff.entries_new,
                "added": diff.added,
                "removed": diff.removed,
                "renamed": diff.renamed
                    .iter()
                    .map(|(from, to)| json!({"from": from, "to": to}))
                    .collect::<Vec<_>>(),
                "rehashed": diff.rehashed
                    .iter()
                    .map(|r| json!({"from": r.name_old, "to": r.name_new}))
                    .collect::<Vec<_>>(),
            })
        );
    } else {
        println!(
            "{}/{}: revision {} -> {} ({} -> {} entries)",
            diff.provider,
            diff.system,
            diff.revision_old,
            diff.revision_new,
            diff.entries_old,
            diff.entries_new
        );
        for name in &diff.added {
            println!("added     {name}");
        }
        for name in &diff.removed {
            println!("removed   {name}");
        }
        for (from, to) in &diff.renamed {
            println!("renamed   {from} -> {to}");
        }
        for r in &diff.rehashed {
            if r.name_old == r.name_new {
                println!("rehashed  {}", r.name_new);
            } else {
                println!("rehashed  {} -> {}", r.name_old, r.name_new);
            }
        }
        if diff.is_empty() {
            println!("no changes");
        }
    }
    Ok(if diff.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

// ---- snapshot ----

/// Shard count for the alias batches. 256 keeps per-shard churn small at
/// MAME scale; at demo scale the 256 identical empty-batch blobs dedupe to
/// ONE tiny CAS object, so small collections pay almost nothing.
const ALIAS_FANOUT: usize = 256;

/// Mint a signed state snapshot (D15): dat-source typing + sharded alias
/// batches into meta/, logged in state.db. `recover` consumes the newest
/// one that verifies under this instance's identity.
pub fn snapshot(env: Env, json: bool) -> anyhow::Result<ExitCode> {
    let identity = crate::config::load_or_create_identity(&env.db_dir)?;
    let now = now_unix();

    let sources: Vec<SourceRef> = env
        .db
        .list_current_sources()?
        .into_iter()
        .map(|(provider, system, dat_blob, imported_at)| SourceRef {
            provider,
            system,
            dat_blob,
            imported_at: u64::try_from(imported_at).unwrap_or(0),
        })
        .collect();

    let mut shards: Vec<Vec<datboi_core::alias::AliasTuple>> = vec![Vec::new(); ALIAS_FANOUT];
    let mut alias_rows: u64 = 0;
    for tuple in env.db.list_alias_tuples()? {
        shards[alias_shard(&tuple.blake3, ALIAS_FANOUT)].push(tuple);
        alias_rows += 1;
    }

    let mut alias_batches = Vec::with_capacity(ALIAS_FANOUT);
    let mut new_batch_blobs: u64 = 0;
    for rows in shards {
        let bytes = AliasBatch { rows }.encode()?;
        let (hash, aliases, outcome) = env.store.put_new(Namespace::Meta, bytes.as_slice())?;
        if outcome == datboi_store_fs::PutOutcome::Stored {
            new_batch_blobs += 1;
        }
        let blob_id =
            env.db
                .upsert_blob(&hash, Some(aliases.size), NsRow::Meta, Residency::Resident)?;
        env.db.insert_aliases(blob_id, &aliases)?;
        env.db.set_verified(blob_id, now)?;
        alias_batches.push(hash);
    }

    // Analysis provenance batches (D48), sharded by the row's blob hash
    // with the same fanout/dedup behavior as aliases. Omitted entirely
    // while no analyzer has ever run (fields absent from the payload).
    let analysis_rows_all = env.db.list_analysis_rows()?;
    let analysis_row_count = analysis_rows_all.len() as u64;
    let mut analysis_batches = Vec::new();
    if !analysis_rows_all.is_empty() {
        let mut shards: Vec<Vec<datboi_core::snapshot::AnalysisRow>> =
            vec![Vec::new(); ALIAS_FANOUT];
        for row in analysis_rows_all {
            shards[alias_shard(&row.blob, ALIAS_FANOUT)].push(row);
        }
        for rows in shards {
            let bytes = AnalysisBatch { rows }.encode()?;
            let (hash, aliases, outcome) = env.store.put_new(Namespace::Meta, bytes.as_slice())?;
            if outcome == datboi_store_fs::PutOutcome::Stored {
                new_batch_blobs += 1;
            }
            let blob_id =
                env.db
                    .upsert_blob(&hash, Some(aliases.size), NsRow::Meta, Residency::Resident)?;
            env.db.insert_aliases(blob_id, &aliases)?;
            env.db.set_verified(blob_id, now)?;
            analysis_batches.push(hash);
        }
    }

    let sequence = env.db.next_snapshot_seq()?;
    let analysis_fanout = if analysis_batches.is_empty() {
        0
    } else {
        ALIAS_FANOUT
    };
    // Tags + authoritative config ride inline (dozens of tiny rows):
    // recovery keeps view definitions and their D33 flips.
    let mut tags = env.db.list_tags()?;
    tags.sort_by(|a, b| a.0.cmp(&b.0));
    let mut config = env.db.config_list_prefix("")?;
    config.sort_by(|a, b| a.0.cmp(&b.0));
    let payload = SnapshotPayload {
        sequence: u64::try_from(sequence).unwrap_or(0),
        created_at: u64::try_from(now).unwrap_or(0),
        sources,
        alias_fanout: ALIAS_FANOUT,
        alias_batches,
        analysis_fanout,
        analysis_batches,
        tags,
        config,
    };
    let bytes = payload.encode_signed(&identity)?;
    let (hash, aliases, _outcome) = env.store.put_new(Namespace::Meta, bytes.as_slice())?;
    let blob_id =
        env.db
            .upsert_blob(&hash, Some(aliases.size), NsRow::Meta, Residency::Resident)?;
    env.db.insert_aliases(blob_id, &aliases)?;
    env.db.set_verified(blob_id, now)?;
    let logged = env.db.snapshot_log_append(&hash, now)?;
    anyhow::ensure!(
        logged == sequence,
        "snapshot_log assigned seq {logged}, object was minted with {sequence} (concurrent snapshot?)"
    );

    if json {
        println!(
            "{}",
            json!({
                "snapshot": hash.to_hex(),
                "sequence": sequence,
                "sources": payload.sources.len(),
                "alias_rows": alias_rows,
                "alias_batches": ALIAS_FANOUT,
                "analysis_rows": analysis_row_count,
                "new_batch_blobs": new_batch_blobs,
            })
        );
    } else {
        println!("snapshot {hash} (seq {sequence})");
        println!(
            "{} source(s), {alias_rows} alias row(s) in {ALIAS_FANOUT} batch(es) ({new_batch_blobs} new)",
            payload.sources.len()
        );
    }
    Ok(ExitCode::SUCCESS)
}

// ---- evict / materialize (M3: the residency planner surface) ----

/// Evict recipe-covered literals until at most `target_bytes` of
/// resident data remain. Every drop passes the D25/D21 safety rules in
/// datboi-exec; `--dry-run` reports the plan without deleting.
pub fn evict(
    env: Env,
    target_bytes: u64,
    license: bool,
    dry_run: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let exec = datboi_exec::Executor::new(&env.store, datboi_exec::ExecConfig::default())?;
    if dry_run {
        let mut evictable: Vec<(String, u64)> = Vec::new();
        let mut blocked: Vec<(String, Vec<String>)> = Vec::new();
        for candidate in env.db.list_eviction_candidates()? {
            if env.db.is_evictable(candidate.blob_id)? {
                evictable.push((candidate.hash.to_hex(), candidate.size.unwrap_or(0)));
            } else {
                blocked.push((
                    candidate.hash.to_hex(),
                    exec.explain_eviction(&env.db, &candidate.hash)?,
                ));
            }
        }
        let reclaimable: u64 = evictable.iter().map(|(_, s)| s).sum();
        if json {
            println!(
                "{}",
                json!({
                    "dry_run": true,
                    "evictable": evictable.len(),
                    "reclaimable_bytes": reclaimable,
                    "blocked": blocked.iter().map(|(h, why)| json!({
                        "hash": h,
                        "reasons": why,
                    })).collect::<Vec<_>>(),
                    "blobs": evictable.iter().map(|(h, s)| json!({"hash": h, "bytes": s})).collect::<Vec<_>>(),
                })
            );
        } else {
            println!(
                "dry run: {} blob(s) evictable, {reclaimable} byte(s) reclaimable, {} candidate(s) blocked",
                evictable.len(),
                blocked.len()
            );
            for (hash, reasons) in &blocked {
                println!("  blocked {hash}:");
                for reason in reasons {
                    println!("    - {reason}");
                }
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    // The drop critical section runs under the D72 singleton guard:
    // this CLI racing the daemon's watermark eviction (or another CLI)
    // is exactly the jointly-stranded-pair hazard the guard exists for.
    let holder = mint_guard_holder()?;
    anyhow::ensure!(
        env.db.claim_gc_guard(&holder, gc_now(), GC_GUARD_TTL_SECS)?,
        "gc guard busy (the daemon is evicting or a gc apply is running); retry shortly"
    );
    let report = exec.evict_covered(&env.db, target_bytes, license);
    env.db.release_gc_guard(&holder)?;
    let report = report?;
    // Every blocked blob gets its reasons spelled out — "0 evicted" with
    // no explanation is how a residency planner loses trust.
    let mut blocked: Vec<(String, Vec<String>)> = Vec::with_capacity(report.blocked.len());
    for (hash, _why) in &report.blocked {
        blocked.push((hash.to_hex(), exec.explain_eviction(&env.db, hash)?));
    }
    if json {
        println!(
            "{}",
            json!({
                "evicted": report.evicted,
                "bytes_reclaimed": report.bytes_reclaimed,
                "replays": report.replays,
                "blocked": blocked.iter().map(|(h, why)| json!({
                    "hash": h,
                    "reasons": why,
                })).collect::<Vec<_>>(),
            })
        );
    } else {
        println!(
            "evicted {} blob(s), reclaimed {} byte(s) ({} licensing replay(s), {} blocked)",
            report.evicted,
            report.bytes_reclaimed,
            report.replays,
            blocked.len()
        );
        for (hash, reasons) in &blocked {
            println!("  blocked {hash}:");
            for reason in reasons {
                println!("    - {reason}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Rematerialize a blob by replaying its recipe route (D25 machinery in
/// reverse: recipes serve bytes back).
pub fn materialize(env: Env, hash_hex: &str, json: bool) -> anyhow::Result<ExitCode> {
    let hash: datboi_core::hash::Blake3 = hash_hex
        .parse()
        .map_err(|_| anyhow::anyhow!("{hash_hex:?} is not a blake3 hex hash"))?;
    let exec = datboi_exec::Executor::new(&env.store, datboi_exec::ExecConfig::default())?;
    exec.materialize(&env.db, &hash)?;
    if json {
        println!("{}", json!({"materialized": hash.to_hex()}));
    } else {
        println!("materialized {hash}");
    }
    Ok(ExitCode::SUCCESS)
}

// ---- gc (D72/D73) ----

fn mint_guard_holder() -> anyhow::Result<datboi_index::GuardHolder> {
    let mut holder = [0u8; 16];
    getrandom::getrandom(&mut holder)?;
    Ok(datboi_index::GuardHolder(holder))
}

fn gc_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Guard TTL for CLI-held critical sections — same bound the daemon
/// uses (a crashed CLI stalls background eviction at most this long).
const GC_GUARD_TTL_SECS: i64 = 15 * 60;

pub fn gc_list(env: &Env, json: bool) -> anyhow::Result<ExitCode> {
    use datboi_exec::policy;
    let grace = policy::grace_secs(&env.db)?;
    let keeps = policy::keep_set(&env.db)?;
    let candidates = env.db.list_orphan_candidates(gc_now(), grace)?;
    if json {
        println!(
            "{}",
            json!({
                "grace_secs": grace,
                "orphans": candidates.iter().map(|c| json!({
                    "hash": c.hash.to_hex(),
                    "size": c.size,
                    "marked_at": c.marked_at,
                    "sources": c.sources,
                    "kept": keeps.contains(&c.hash),
                })).collect::<Vec<_>>(),
            })
        );
        return Ok(ExitCode::SUCCESS);
    }
    if candidates.is_empty() {
        println!("no reviewable orphan candidates (grace {grace}s)");
        return Ok(ExitCode::SUCCESS);
    }
    let mut reclaimable = 0u64;
    for c in &candidates {
        let kept = keeps.contains(&c.hash);
        if !kept {
            reclaimable += c.size.unwrap_or(0);
        }
        println!(
            "{} {:>12} {}{}",
            c.hash.to_hex(),
            c.size.unwrap_or(0),
            if kept { "[kept] " } else { "" },
            c.sources.join(", ")
        );
    }
    println!(
        "{} candidate(s); {} byte(s) reclaimable via `datboi gc apply`",
        candidates.len(),
        reclaimable
    );
    Ok(ExitCode::SUCCESS)
}

pub fn gc_keep(env: &Env, hash_hex: &str, keep: bool) -> anyhow::Result<ExitCode> {
    let hash: datboi_core::hash::Blake3 = hash_hex
        .parse()
        .map_err(|_| anyhow::anyhow!("{hash_hex:?} is not a blake3 hex hash"))?;
    datboi_exec::policy::set_keep(&env.db, &hash, keep)?;
    println!("{} {hash}", if keep { "kept" } else { "unkept" });
    Ok(ExitCode::SUCCESS)
}

/// The CLI apply mirrors the daemon's discipline exactly: guard, then
/// delete-time re-verification per blob, bytes before rows.
pub fn gc_apply(mut env: Env, hashes: &[String], json: bool) -> anyhow::Result<ExitCode> {
    use datboi_exec::policy;
    let exec = datboi_exec::Executor::new(&env.store, datboi_exec::ExecConfig::default())?;
    let grace = policy::grace_secs(&env.db)?;
    let keeps = policy::keep_set(&env.db)?;
    let roots = exec.orphan_extra_roots(&env.db)?;
    let now = gc_now();
    let mut wanted = env.db.list_orphan_candidates(now, grace)?;
    if !hashes.is_empty() {
        let requested: std::collections::HashSet<&str> =
            hashes.iter().map(String::as_str).collect();
        wanted.retain(|c| requested.contains(c.hash.to_hex().as_str()));
    }
    let holder = mint_guard_holder()?;
    anyhow::ensure!(
        env.db.claim_gc_guard(&holder, now, GC_GUARD_TTL_SECS)?,
        "gc guard busy (the daemon is evicting or another apply is running); retry shortly"
    );
    let mut deleted = 0u64;
    let mut bytes = 0u64;
    let mut skipped = 0u64;
    let result: anyhow::Result<()> = (|| {
        for c in &wanted {
            if keeps.contains(&c.hash)
                || !env.db.orphan_still_deletable(c.blob_id, &roots, now, grace)?
            {
                skipped += 1;
                continue;
            }
            env.store
                .remove_blob(datboi_store_fs::Namespace::Data, &c.hash)?;
            env.db.delete_orphan_rows(c.blob_id)?;
            deleted += 1;
            bytes += c.size.unwrap_or(0);
        }
        Ok(())
    })();
    env.db.release_gc_guard(&holder)?;
    result?;
    if json {
        println!(
            "{}",
            json!({"deleted": deleted, "bytes_reclaimed": bytes, "skipped": skipped})
        );
    } else {
        println!(
            "deleted {deleted} orphan(s), reclaimed {bytes} byte(s), {skipped} skipped by delete-time re-verification"
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn gc_config(
    env: &Env,
    high_water: Option<&str>,
    low_water: Option<&str>,
    grace_secs: Option<i64>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    use datboi_exec::policy;
    for (key, value) in [
        (policy::KEY_HIGH_WATER, high_water),
        (policy::KEY_LOW_WATER, low_water),
    ] {
        if let Some(value) = value {
            // Validate on write (the read side falls back to defaults
            // rather than obeying a typo).
            anyhow::ensure!(
                value.eq_ignore_ascii_case("off")
                    || value.strip_suffix('%').is_some_and(|p| p.parse::<u8>().is_ok_and(|n| n <= 100))
                    || value.parse::<u64>().is_ok(),
                "{value:?}: expected \"off\", \"NN%\", or absolute bytes"
            );
            env.db.config_set(key, value.as_bytes())?;
        }
    }
    if let Some(grace) = grace_secs {
        anyhow::ensure!(grace >= 0, "grace must be non-negative");
        env.db
            .config_set(policy::KEY_GRACE_SECS, grace.to_string().as_bytes())?;
    }
    let (high, low, grace) = (
        policy::high_water(&env.db)?,
        policy::low_water(&env.db)?,
        policy::grace_secs(&env.db)?,
    );
    if json {
        println!(
            "{}",
            json!({"high_water": format!("{high:?}"), "low_water": format!("{low:?}"), "grace_secs": grace})
        );
    } else {
        println!("high-water: {high:?}\nlow-water:  {low:?}\ngrace:      {grace}s");
    }
    Ok(ExitCode::SUCCESS)
}

// ---- sweep ----

/// One refinement sweep round (D45): enqueue unanalyzed data blobs for
/// the named analyzer (dat-blind), bump dat-matched priorities
/// (dat-aware ordering, D47), process up to `limit` items, and record
/// provenance including negatives (D48).
pub fn sweep(
    mut env: Env,
    analyzer_name: &str,
    limit: usize,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let mut analyzer: Box<dyn datboi_ingest::refine::Analyzer> = match analyzer_name {
        "noop" | "noop/1" => Box::new(datboi_ingest::refine::NoopAnalyzer),
        "chunk" | "fastcdc" => Box::new(datboi_ingest::analyzers::ChunkAnalyzer),
        "preflate" | "preflate-split" => {
            Box::new(datboi_ingest::analyzers::PreflateZipAnalyzer::new())
        }
        "ecm" => Box::new(datboi_ingest::analyzers::EcmAnalyzer::new()),
        other => {
            anyhow::bail!("unknown analyzer {other:?} (available: noop, chunk, preflate, ecm)")
        }
    };
    let report =
        datboi_ingest::refine::run_sweep(&mut env.db, &env.store, analyzer.as_mut(), limit)?;
    if report.disabled {
        let family = analyzer.family();
        if json {
            println!("{}", json!({"analyzer": analyzer.name(), "disabled": true}));
        } else {
            println!(
                "analyzer family {family:?} is disabled (D60): `datboi analyzer enable {family}`"
            );
        }
        return Ok(ExitCode::from(1));
    }
    let remaining = env.db.sweep_queue_len(&analyzer.id())?;
    if json {
        println!(
            "{}",
            json!({
                "analyzer": analyzer.name(),
                "analyzer_id": analyzer.id().to_hex(),
                "enqueued": report.enqueued,
                "analyzed": report.analyzed,
                "positive": report.positive,
                "negative": report.negative,
                "errors": report.errors.iter().map(|(h, e)| json!({"blob": h.to_hex(), "error": e})).collect::<Vec<_>>(),
                "queue_remaining": remaining,
            })
        );
    } else {
        println!(
            "sweep {}: {} enqueued, {} analyzed ({} positive, {} negative), {} error(s), {} queued",
            analyzer.name(),
            report.enqueued,
            report.analyzed,
            report.positive,
            report.negative,
            report.errors.len(),
            remaining
        );
        for (hash, error) in &report.errors {
            println!("error: {hash}: {error}");
        }
    }
    Ok(if report.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// The shipped analyzer families (D60 config keys). Adding an analyzer
/// is adding a row here + a `family()` on its impl.
const ANALYZER_FAMILIES: &[&str] = &["noop", "chunk", "preflate", "ecm"];

fn require_family(name: &str) -> anyhow::Result<&str> {
    ANALYZER_FAMILIES
        .iter()
        .find(|f| **f == name)
        .copied()
        .with_context(|| {
            format!(
                "unknown analyzer family {name:?} (available: {})",
                ANALYZER_FAMILIES.join(", ")
            )
        })
}

pub fn analyzer_list(env: &Env, json: bool) -> anyhow::Result<ExitCode> {
    use datboi_ingest::refine::{analyzer_enabled, analyzer_params};
    let mut rows = Vec::new();
    for family in ANALYZER_FAMILIES {
        let enabled = analyzer_enabled(&env.db, family)?;
        let params = analyzer_params(&env.db, family)?;
        rows.push((family, enabled, params));
    }
    if json {
        println!(
            "{}",
            json!({"analyzers": rows.iter().map(|(f, enabled, params)| json!({
                "family": f,
                "enabled": enabled,
                "params_hex": params.as_ref().map(|p| p.iter().map(|b| format!("{b:02x}")).collect::<String>()),
            })).collect::<Vec<_>>()})
        );
    } else {
        for (family, enabled, params) in &rows {
            println!(
                "{family:<10} {}  params: {}",
                if *enabled { "enabled " } else { "DISABLED" },
                params
                    .as_ref()
                    .map_or_else(|| "-".into(), |p| format!("{} bytes", p.len())),
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn analyzer_set_enabled(env: &Env, name: &str, enabled: bool) -> anyhow::Result<ExitCode> {
    let family = require_family(name)?;
    datboi_ingest::refine::set_analyzer_enabled(&env.db, family, enabled)?;
    println!(
        "analyzer {family}: {}",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(ExitCode::SUCCESS)
}

pub fn analyzer_set_params(env: &Env, name: &str, hex: Option<&str>) -> anyhow::Result<ExitCode> {
    let family = require_family(name)?;
    let bytes = match hex {
        Some(h) => {
            anyhow::ensure!(
                h.len().is_multiple_of(2) && h.chars().all(|c| c.is_ascii_hexdigit()),
                "params must be an even-length hex string"
            );
            Some(
                (0..h.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&h[i..i + 2], 16).expect("validated hex"))
                    .collect::<Vec<u8>>(),
            )
        }
        None => None,
    };
    datboi_ingest::refine::set_analyzer_params(&env.db, family, bytes.as_deref())?;
    println!(
        "analyzer {family}: params {}",
        match &bytes {
            Some(b) => format!("set ({} bytes)", b.len()),
            None => "cleared".into(),
        }
    );
    Ok(ExitCode::SUCCESS)
}

// ---- recover ----

/// Fast-walk parallelism. Structure is what matters now; the number gets
/// tuned by the M1 NFS bench (90-roadmap.md) when the bench machine
/// exists.
const RECOVER_WALK_WORKERS: usize = 8;

/// Recovery (D15): rebuilds the blob index and the recipe index from
/// the store. Two data-pass modes:
///
/// * **fast (default when a snapshot authenticates)** — a parallel
///   metadata-only walk: hash from the file name, size from stat, no
///   bytes read. Aliases come back from the snapshot's sharded alias
///   batches (D43); blobs the batches don't cover (ingested after the
///   snapshot) get theirs from the next `scrub`, which also refreshes
///   `verified_at` — byte verification is demoted to scrub, not paid
///   at recovery time. Days-over-NFS becomes minutes.
/// * **full-read (fallback)** — without an identity key or a verifying
///   snapshot, the original one-pass re-hash rebuilds aliases from the
///   bytes themselves.
///
/// Recovered recipes have no verification provenance, so they re-enter
/// as Pending and re-verify lazily. Catalog tables come back by
/// replaying `dat import` from the snapshot's CAS dat blobs.
pub fn recover(mut env: Env, json: bool) -> anyhow::Result<ExitCode> {
    env.db.truncate_cache()?;
    let now = now_unix();

    let mut blobs: u64 = 0;
    let mut corrupt: Vec<String> = Vec::new();
    let mut foreign: u64 = 0;

    // meta/: recipes and other structured objects (bytes must be parsed
    // anyway, and meta is small — this pass stays a verifying read).
    let mut recipes: u64 = 0;
    let mut meta_unknown: u64 = 0;
    let mut snapshot_candidates: Vec<(datboi_core::hash::Blake3, Vec<u8>)> = Vec::new();
    for item in env.store.list(Namespace::Meta) {
        match item {
            Ok((hash, _size)) => {
                let mut file = env
                    .store
                    .get(Namespace::Meta, &hash)?
                    .context("listed blob vanished mid-recover")?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                let mut hasher = AliasHasher::new();
                hasher.update(&bytes);
                let aliases = hasher.finalize();
                if aliases.blake3 != hash {
                    corrupt.push(hash.to_hex());
                    continue;
                }
                let blob_id = env.db.upsert_blob(
                    &hash,
                    Some(aliases.size),
                    NsRow::Meta,
                    Residency::Resident,
                )?;
                env.db.insert_aliases(blob_id, &aliases)?;
                env.db.set_verified(blob_id, now)?;
                match object::sniff(&bytes) {
                    Some((ObjectKind::Recipe, _, _)) => {
                        let recipe = Recipe::decode(&bytes)?;
                        index_recovered_recipe(&mut env.db, blob_id, &recipe)?;
                        recipes += 1;
                    }
                    Some((ObjectKind::StateSnapshot, _, _)) => {
                        snapshot_candidates.push((hash, bytes.clone()));
                    }
                    _ => meta_unknown += 1,
                }
            }
            Err(StoreError::Foreign { .. }) => foreign += 1,
            Err(e) => return Err(e.into()),
        }
    }

    // Snapshot-driven catalog recovery: newest snapshot that verifies under
    // OUR identity (an attacker who can write meta/ can mint self-consistent
    // snapshots under their own key, so an unpinned signature proves nothing).
    let mut dats_reimported: u64 = 0;
    let mut analysis_restored: u64 = 0;
    let mut aliases_restored: u64 = 0;
    let mut snapshot_seq: Option<u64> = None;
    let mut fast_walk = false;
    let mut snapshot_note: &str =
        "no usable state snapshot: re-run `datboi dat import` for each dat to restore audits";
    let selected = match crate::config::load_identity(&env.db_dir)? {
        None => {
            snapshot_note = "no identity key: cannot authenticate snapshots; \
                re-run `datboi dat import` for each dat to restore audits";
            None
        }
        Some(identity) => {
            let pk = identity.public_key();
            snapshot_candidates
                .iter()
                .filter_map(|(hash, b)| Some((*hash, StateSnapshot::decode(b).ok()?)))
                .filter(|(_, snap)| snap.verify(&pk).is_ok())
                .max_by_key(|(_, snap)| snap.payload.sequence)
        }
    };

    // data/: fast metadata-only walk when a snapshot authenticates
    // (aliases restored from its batches below; verification demoted to
    // scrub), full-read re-hash otherwise.
    // Batched: per-row autocommits dominate recovery wall-clock (the
    // smoke bench measured ~9s of fsync churn per 50k rows). One
    // transaction for the whole data pass — a crash mid-pass re-runs
    // recover from the truncate anyway. (The meta pass stays unbatched:
    // insert_recipe manages its own transaction.)
    let tx = env.db.cache().unchecked_transaction()?;
    if selected.is_some() {
        fast_walk = true;
        for item in env
            .store
            .list_parallel(Namespace::Data, RECOVER_WALK_WORKERS)
        {
            match item {
                Ok((hash, size)) => {
                    env.db
                        .upsert_blob(&hash, Some(size), NsRow::Data, Residency::Resident)?;
                    blobs += 1;
                }
                Err(StoreError::Foreign { .. }) => foreign += 1,
                Err(e) => return Err(e.into()),
            }
        }
    } else {
        for item in env.store.list(Namespace::Data) {
            match item {
                Ok((hash, _size)) => {
                    let mut file = env
                        .store
                        .get(Namespace::Data, &hash)?
                        .context("listed blob vanished mid-recover")?;
                    let mut hasher = AliasHasher::new();
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        let n = file.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        hasher.update(&buf[..n]);
                    }
                    let aliases = hasher.finalize();
                    if aliases.blake3 != hash {
                        corrupt.push(hash.to_hex());
                        // Index the row (the file exists) but grant it no
                        // verification and no aliases.
                        env.db.upsert_blob(
                            &hash,
                            Some(aliases.size),
                            NsRow::Data,
                            Residency::Resident,
                        )?;
                        continue;
                    }
                    let blob_id = env.db.upsert_blob(
                        &hash,
                        Some(aliases.size),
                        NsRow::Data,
                        Residency::Resident,
                    )?;
                    env.db.insert_aliases(blob_id, &aliases)?;
                    env.db.set_verified(blob_id, now)?;
                    blobs += 1;
                }
                Err(StoreError::Foreign { .. }) => foreign += 1,
                Err(e) => return Err(e.into()),
            }
        }
    }

    tx.commit()?;

    {
        {
            if let Some((snap_hash, snap)) = selected {
                // Alias restore (D43, the fast-walk counterpart of the
                // full-read pass): batch rows for blobs the walk found
                // get their alias tuples back without reading bytes.
                // Rows for vanished blobs are skipped; blobs newer than
                // the snapshot get aliases from the next scrub.
                if fast_walk {
                    let tx = env.db.cache().unchecked_transaction()?;
                    for batch_hash in &snap.payload.alias_batches {
                        let Some(mut file) = env.store.get(Namespace::Meta, batch_hash)? else {
                            eprintln!(
                                "warning: snapshot references alias batch {batch_hash} but it is not in the store"
                            );
                            continue;
                        };
                        let mut bytes = Vec::new();
                        file.read_to_end(&mut bytes)?;
                        match AliasBatch::decode(&bytes) {
                            Ok(batch) => {
                                for tuple in &batch.rows {
                                    if let Some(row) = env.db.blob_by_hash(&tuple.blake3)? {
                                        env.db.insert_aliases(row.blob_id, tuple)?;
                                        aliases_restored += 1;
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("warning: alias batch {batch_hash} does not decode: {e}");
                            }
                        }
                    }
                    tx.commit()?;
                }
                for source in &snap.payload.sources {
                    let Some(mut file) = env.store.get(Namespace::Data, &source.dat_blob)? else {
                        eprintln!(
                            "warning: snapshot references dat blob {} for {}/{} but it is not in the store",
                            source.dat_blob, source.provider, source.system
                        );
                        continue;
                    };
                    let mut dat_bytes = Vec::new();
                    file.read_to_end(&mut dat_bytes)?;
                    import_dat(
                        &env.store,
                        &mut env.db,
                        &dat_bytes,
                        &ImportOptions {
                            provider: Some(&source.provider),
                            system: Some(&source.system),
                            imported_at: i64::try_from(source.imported_at).unwrap_or(0),
                        },
                    )?;
                    dats_reimported += 1;
                }
                // Analysis provenance (D48): restore rows from the
                // snapshot's batches so recovery never re-pays expensive
                // analysis — negatives included. Rows for blobs the scan
                // didn't find are skipped (bytes gone ⇒ provenance moot).
                for batch_hash in &snap.payload.analysis_batches {
                    let Some(mut file) = env.store.get(Namespace::Meta, batch_hash)? else {
                        eprintln!(
                            "warning: snapshot references analysis batch {batch_hash} but it is not in the store"
                        );
                        continue;
                    };
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes)?;
                    match AnalysisBatch::decode(&bytes) {
                        Ok(batch) => {
                            for row in &batch.rows {
                                if env.db.restore_analysis_row(row, now_unix())? {
                                    analysis_restored += 1;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("warning: analysis batch {batch_hash} does not decode: {e}");
                        }
                    }
                }
                // Tags + config (views: definitions and their D33
                // flips) come back verbatim from the signed payload.
                for (key, value) in &snap.payload.config {
                    env.db.config_set(key, value)?;
                }
                for (name, hash) in &snap.payload.tags {
                    env.db.set_tag(
                        name,
                        hash,
                        i64::try_from(snap.payload.created_at).unwrap_or(0),
                    )?;
                }
                // Sequence monotonicity survives the nuke: re-seed the log
                // so the next mint continues from here instead of reusing
                // this snapshot's sequence.
                env.db.snapshot_log_restore(
                    i64::try_from(snap.payload.sequence).unwrap_or(i64::MAX),
                    &snap_hash,
                    i64::try_from(snap.payload.created_at).unwrap_or(0),
                )?;
                snapshot_seq = Some(snap.payload.sequence);
                snapshot_note = "catalog restored from the state snapshot (dat imports replayed)";
            }
        }
    }

    let notes = [
        "recipes recovered as Pending (no verification provenance); they re-verify lazily",
        snapshot_note,
        "rescan cache is empty: the next ingest re-reads sources",
    ];
    if json {
        println!(
            "{}",
            json!({
                "blobs_indexed": blobs,
                "recipes_indexed": recipes,
                "meta_other": meta_unknown,
                "foreign_files": foreign,
                "snapshot_seq": snapshot_seq,
                "fast_walk": fast_walk,
                "aliases_restored": aliases_restored,
                "dats_reimported": dats_reimported,
                "analysis_restored": analysis_restored,
                "corrupt": corrupt,
                "notes": notes,
            })
        );
    } else {
        println!("blobs indexed      {blobs:>8}");
        println!("recipes indexed    {recipes:>8}");
        if let Some(seq) = snapshot_seq {
            println!("snapshot used      {seq:>8}");
            println!("dats re-imported   {dats_reimported:>8}");
            println!("analysis restored  {analysis_restored:>8}");
            println!("aliases restored   {aliases_restored:>8}");
        }
        if fast_walk {
            println!("mode: fast (metadata-only walk; run `datboi scrub` to re-verify bytes)");
        }
        if meta_unknown > 0 {
            println!("other meta objects {meta_unknown:>8}");
        }
        if foreign > 0 {
            println!("foreign files      {foreign:>8}");
        }
        for hash in &corrupt {
            println!("CORRUPT: {hash}");
        }
        for note in notes {
            println!("note: {note}");
        }
    }
    Ok(if corrupt.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn index_recovered_recipe(
    db: &mut datboi_index::Db,
    recipe_blob_id: i64,
    recipe: &Recipe,
) -> anyhow::Result<()> {
    let (op_kind, op_name, seek_class) = match &recipe.op {
        Op::Builtin { name, major } => {
            let full = format!("{name}@{major}");
            // Conservative inference; docs/80-views.md classes. Unknown
            // builtins default to Opaque (never lies toward seekability).
            let class = if name == "assemble" || name == "swap" {
                SeekClass::Affine
            } else {
                SeekClass::Opaque
            };
            (OpKind::Builtin, full, class)
        }
        Op::Wasm {
            component, export, ..
        } => (
            OpKind::Wasm,
            format!("{}#{export}", component.to_hex()),
            SeekClass::Opaque,
        ),
    };
    let mut inputs = Vec::new();
    for (position, input) in recipe.inputs.iter().enumerate() {
        let id = ensure_blob(db, &input.hash)?;
        inputs.push((
            u32::try_from(position).expect("recipe input count fits u32"),
            id,
            input.role.as_deref(),
        ));
    }
    let mut outputs = Vec::new();
    for (ordinal, output) in recipe.outputs.iter().enumerate() {
        let id = ensure_blob(db, &output.hash)?;
        outputs.push((
            u32::try_from(ordinal).expect("recipe output count fits u32"),
            id,
            output.size,
            output.name.as_deref(),
        ));
    }
    db.insert_recipe(&NewRecipe {
        blob_id: recipe_blob_id,
        op_kind,
        op_name: &op_name,
        seek_class,
        source: RecipeSource::LocalIngest,
        inputs: &inputs,
        outputs: &outputs,
    })?;
    Ok(())
}

/// Referenced-but-absent blobs get Absent rows (peer/member semantics).
fn ensure_blob(db: &datboi_index::Db, hash: &datboi_core::hash::Blake3) -> anyhow::Result<i64> {
    if let Some(id) = db.get_blob_id(hash)? {
        return Ok(id);
    }
    Ok(db.upsert_blob(hash, None, NsRow::Data, Residency::Absent)?)
}

// ---- scrub ----

pub fn scrub(
    env: &Env,
    sample_pct: u8,
    rehabilitate: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let pct = sample_pct.min(100);
    let now = now_unix();
    let mut checked: u64 = 0;
    let mut refreshed: u64 = 0;
    let mut corrupt: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for ns in [Namespace::Data, Namespace::Meta] {
        for item in env.store.list(ns) {
            let (hash, size) = match item {
                Ok(pair) => pair,
                Err(StoreError::Foreign { .. }) => continue,
                Err(e) => return Err(e.into()),
            };
            // Deterministic sampling by hash prefix: no RNG, same subset
            // every run at a given percentage.
            if u32::from(hash.0[0]) * 100 >= u32::from(pct) * 256 {
                continue;
            }
            checked += 1;
            // The same read computes the full alias tuple, so scrub is
            // also fast-recovery's back-fill: blobs indexed by the
            // metadata-only walk get aliases + verified_at here.
            match env.store.verify_with_aliases(ns, &hash)? {
                (VerifyOutcome::Valid, aliases) => {
                    let ns_row = match ns {
                        Namespace::Data => NsRow::Data,
                        Namespace::Meta => NsRow::Meta,
                    };
                    let blob_id =
                        env.db
                            .upsert_blob(&hash, Some(size), ns_row, Residency::Resident)?;
                    if let Some(aliases) = &aliases {
                        env.db.insert_aliases(blob_id, aliases)?;
                    }
                    env.db.set_verified(blob_id, now)?;
                    refreshed += 1;
                }
                (VerifyOutcome::Corrupt { .. }, _) => corrupt.push(hash.to_hex()),
                (VerifyOutcome::Missing, _) => missing.push(hash.to_hex()),
            }
        }
    }
    // Rehabilitation (D54-era work item): re-execute poisoned recipes;
    // a verified re-replay is the one sanctioned exit from Failed.
    let mut rehabilitated: Vec<i64> = Vec::new();
    let mut still_failed: Vec<(i64, String)> = Vec::new();
    if rehabilitate {
        let exec = datboi_exec::Executor::new(&env.store, datboi_exec::ExecConfig::default())?;
        for recipe_id in env.db.list_failed_recipes()? {
            match exec.rehabilitate(&env.db, recipe_id) {
                Ok(_) => rehabilitated.push(recipe_id),
                Err(e) => still_failed.push((recipe_id, e.to_string())),
            }
        }
    }

    if json {
        println!(
            "{}",
            json!({
                "sample_pct": pct, "checked": checked, "refreshed": refreshed,
                "rehabilitated": rehabilitated,
                "still_failed": still_failed.iter().map(|(id, e)| json!({"recipe": id, "error": e})).collect::<Vec<_>>(),
                "corrupt": corrupt, "missing": missing,
            })
        );
    } else {
        println!("checked {checked} blobs ({pct}% sample), {refreshed} rows refreshed");
        for id in &rehabilitated {
            println!("rehabilitated recipe #{id}");
        }
        for (id, e) in &still_failed {
            println!("recipe #{id} stays poisoned: {e}");
        }
        for h in &corrupt {
            println!("CORRUPT: {h}");
        }
        for h in &missing {
            println!("MISSING: {h}");
        }
        if corrupt.is_empty() && missing.is_empty() {
            println!("no problems found");
        }
    }
    Ok(if corrupt.is_empty() && missing.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

// ---- status ----

pub fn status(env: &Env, json: bool) -> anyhow::Result<ExitCode> {
    let mut ns_stats = Vec::new();
    for ns in [Namespace::Data, Namespace::Meta] {
        let (mut count, mut bytes) = (0u64, 0u64);
        for (_, size) in env.store.list(ns).flatten() {
            count += 1;
            bytes += size;
        }
        ns_stats.push((ns, count, bytes));
    }
    let conn = env.db.cache();
    let table_count = |table: &str| -> anyhow::Result<i64> {
        Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
    };
    let tables = [
        "blob",
        "alias",
        "recipe",
        "entry",
        "rom_claim",
        "content_identity",
    ];
    let mut counts = Vec::new();
    for t in tables {
        counts.push((t, table_count(t)?));
    }
    // Literal-only bytes: resident data with no non-failed rebuild route.
    // The number that sizes the "can't shrink this yet" tax (7z/rar
    // containers, unanalyzed blobs) — watch it fall as analyzers land.
    let literal_only: i64 = conn.query_row(
        "SELECT COALESCE(SUM(b.size), 0) FROM blob b
         WHERE b.namespace = 0 AND b.residency = 0
           AND NOT EXISTS (
             SELECT 1 FROM recipe_output ro
             JOIN recipe r ON r.recipe_id = ro.recipe_id
             WHERE ro.blob_id = b.blob_id AND r.verify != 2)",
        [],
        |r| r.get(0),
    )?;
    let mut sources: Vec<(String, String, Option<i64>)> = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT s.provider, s.system, MAX(r.imported_at)
         FROM dat_source s LEFT JOIN dat_revision r ON r.source_id = s.source_id
         GROUP BY s.source_id ORDER BY s.provider, s.system",
    )?;
    for row in stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))? {
        sources.push(row?);
    }

    if json {
        println!(
            "{}",
            json!({
                "store": ns_stats.iter()
                    .map(|(ns, c, b)| json!({"namespace": ns.dir(), "blobs": c, "bytes": b}))
                    .collect::<Vec<_>>(),
                "db": counts.iter().map(|(t, c)| json!({"table": t, "rows": c})).collect::<Vec<_>>(),
                "literal_only_bytes": literal_only,
                "sources": sources.iter()
                    .map(|(p, s, at)| json!({"provider": p, "system": s, "last_import": at}))
                    .collect::<Vec<_>>(),
            })
        );
    } else {
        for (ns, count, bytes) in &ns_stats {
            println!("{:<5} {count:>8} blobs  {bytes:>12} bytes", ns.dir());
        }
        for (t, c) in &counts {
            println!("{t:<17} {c:>8} rows");
        }
        println!("literal-only      {literal_only:>8} bytes (no rebuild route yet)");
        for (p, s, at) in &sources {
            println!(
                "{p}/{s}  last import {}",
                at.map_or_else(|| "-".into(), |v| v.to_string())
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Link a retool clonelist to a dat source (D57).
pub fn dat_clonelist(env: &Env, source: &str, file: &Path, json: bool) -> anyhow::Result<ExitCode> {
    let (provider, system) = split_source(source)?;
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let report = datboi_catalog::import_clonelist(&env.db, &env.store, provider, system, &bytes)?;
    if json {
        println!(
            "{}",
            json!({
                "source": source,
                "clonelist": report.hash.to_hex(),
                "terms": report.terms,
                "skipped": report.skipped,
            })
        );
    } else {
        println!(
            "clonelist for {source}: {} ({} term(s), {} skipped)",
            report.hash, report.terms, report.skipped
        );
    }
    Ok(ExitCode::SUCCESS)
}

// ---- views (M4: definitions, evaluation, manifests) ----

#[allow(clippy::too_many_arguments)]
pub fn view_define(
    env: &Env,
    name: &str,
    source: &str,
    template: &str,
    selection: Option<datboi_catalog::SelectionPolicy>,
    mame: Option<datboi_catalog::MameMode>,
    profile: Option<String>,
    image: Option<datboi_catalog::ImageParams>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let (provider, system) = split_source(source)?;
    let def = datboi_catalog::ViewDef {
        name: name.to_owned(),
        provider: provider.to_owned(),
        system: system.to_owned(),
        template: template.to_owned(),
        selection,
        profile,
        image,
        mame,
    };
    datboi_catalog::define_view(&env.db, &def)?;
    if json {
        println!(
            "{}",
            json!({
                "view": name,
                "source": source,
                "template": template,
                "selection": def.selection.as_ref().map(|p| json!({
                    "mode": "1g1r", "regions": p.regions, "langs": p.langs,
                })),
                "profile": def.profile,
                "image": def.image.as_ref().map(|i| json!({
                    "cluster_size": i.cluster_size,
                    "partition": i.partition,
                    "label": i.label,
                })),
                "mame_mode": def.mame.map(datboi_catalog::MameMode::as_str),
            })
        );
    } else {
        let sel = match &def.selection {
            Some(p) => format!(
                "1g1r regions=[{}] langs=[{}]",
                p.regions.join(","),
                p.langs.join(",")
            ),
            None => "all".to_owned(),
        };
        let prof = def.profile.as_deref().unwrap_or("none");
        let img = match &def.image {
            Some(i) => format!(
                ", image fat32 (cluster {}, {})",
                i.cluster_size,
                if i.partition { "mbr" } else { "superfloppy" }
            ),
            None => String::new(),
        };
        let mame = match def.mame {
            Some(m) => format!(", mame {}", m.as_str()),
            None => String::new(),
        };
        println!(
            "defined view {name} over {source} (template {template:?}, selection {sel}, profile {prof}{img}{mame})"
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn view_profiles(json: bool) -> anyhow::Result<ExitCode> {
    if json {
        println!(
            "{}",
            json!({"profiles": datboi_catalog::PROFILES.iter().map(|p| json!({
                "name": p.name,
                "max_name_len": p.max_name_len,
                "max_file_size": p.max_file_size,
                "max_dir_entries": p.max_dir_entries,
            })).collect::<Vec<_>>()})
        );
    } else {
        for p in datboi_catalog::PROFILES {
            let size = p
                .max_file_size
                .map_or("unlimited".to_owned(), |s| format!("{s} B max"));
            let entries = p
                .max_dir_entries
                .map_or("unlimited".to_owned(), |n| format!("{n}/dir"));
            println!(
                "{:<12} names \u{2264}{} chars, files {size}, entries {entries}",
                p.name, p.max_name_len
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn view_eval(mut env: Env, name: &str, json: bool) -> anyhow::Result<ExitCode> {
    let def = datboi_catalog::get_view(&env.db, name)?
        .with_context(|| format!("view {name:?} is not defined"))?;
    let report = datboi_catalog::evaluate_view(&mut env.db, &env.store, &def, now_unix())?;
    if json {
        println!(
            "{}",
            json!({
                "view": name,
                "snapshot": report.snapshot.to_hex(),
                "rows": report.rows,
                "missing_claims": report.missing,
                "disambiguated": report.disambiguated,
                "families": report.families,
                "skipped_oversize": report.skipped_oversize,
                "bucketed_dirs": report.bucketed_dirs,
                "overfull_dirs": report.overfull_dirs,
                "dangling_device_refs": report.dangling_device_refs,
            })
        );
    } else {
        let mut extras = String::new();
        if let Some(families) = report.families {
            extras.push_str(&format!(", {families} famil(ies)"));
        }
        if report.skipped_oversize > 0 {
            extras.push_str(&format!(
                ", {} oversize row(s) SKIPPED",
                report.skipped_oversize
            ));
        }
        if report.bucketed_dirs > 0 {
            extras.push_str(&format!(
                ", {} dir(s) alpha-bucketed to fit the entry cap",
                report.bucketed_dirs
            ));
        }
        if report.overfull_dirs > 0 {
            extras.push_str(&format!(
                ", {} dir(s) STILL over the profile entry cap",
                report.overfull_dirs
            ));
        }
        if report.dangling_device_refs > 0 {
            extras.push_str(&format!(
                ", {} dangling device_ref(s)",
                report.dangling_device_refs
            ));
        }
        println!(
            "view {name}: snapshot {} ({} row(s), {} claim(s) missing, {} path(s) disambiguated{extras})",
            report.snapshot, report.rows, report.missing, report.disambiguated
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn view_list(env: &Env, json: bool) -> anyhow::Result<ExitCode> {
    let mut items = Vec::new();
    for name in datboi_catalog::list_views(&env.db)? {
        let snap = env.db.get_tag(&format!("view/{name}"))?;
        items.push((name, snap));
    }
    if json {
        println!(
            "{}",
            json!({"views": items.iter().map(|(n, s)| json!({
                "name": n,
                "snapshot": s.map(|h| h.to_hex()),
            })).collect::<Vec<_>>()})
        );
    } else if items.is_empty() {
        println!("no views defined");
    } else {
        for (name, snap) in &items {
            println!(
                "{name}  {}",
                snap.map_or_else(|| "(never evaluated)".into(), |h| h.to_hex())
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve a view's current snapshot (tag → decoded manifest).
fn load_view_snapshot(
    env: &Env,
    name: &str,
) -> anyhow::Result<(
    datboi_core::hash::Blake3,
    datboi_core::viewsnap::ViewSnapshot,
)> {
    let snap_hash = env
        .db
        .get_tag(&format!("view/{name}"))?
        .with_context(|| format!("view {name:?} has no snapshot; run `view eval {name}`"))?;
    let mut bytes = Vec::new();
    env.store
        .get(Namespace::Meta, &snap_hash)?
        .with_context(|| format!("snapshot blob {snap_hash} missing from meta/"))?
        .read_to_end(&mut bytes)?;
    let snap = datboi_core::viewsnap::ViewSnapshot::decode(&bytes)
        .map_err(|e| anyhow::anyhow!("snapshot does not decode: {e}"))?;
    Ok((snap_hash, snap))
}

pub fn view_manifest(env: &Env, name: &str, json: bool) -> anyhow::Result<ExitCode> {
    let (snap_hash, snap) = load_view_snapshot(env, name)?;
    if json {
        println!(
            "{}",
            json!({
                "view": name,
                "snapshot": snap_hash.to_hex(),
                "created_at": snap.created_at,
                "rows": snap.rows.iter().map(|r| json!({
                    "path": r.path, "hash": r.hash.to_hex(),
                    "size": r.size, "seek": r.seek,
                })).collect::<Vec<_>>(),
            })
        );
    } else {
        println!(
            "view {name} snapshot {snap_hash} ({} rows)",
            snap.rows.len()
        );
        for r in &snap.rows {
            println!("{:>12}  {}  {}", r.size, r.hash, r.path);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Marker suffix for in-flight writes; leftovers from a crashed sync
/// are ours to clean.
const SYNC_TMP_SUFFIX: &str = ".datboi-tmp";

/// SD sync (80-views.md): materialize a snapshot into a plain directory
/// for flashcart cards. Incremental by (path, size) — `--verify`
/// re-hashes matches; `--delete` removes extraneous files. All bytes
/// flow through the executor's verified range path.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub fn view_sync(
    env: &Env,
    name: &str,
    target: &Path,
    delete: bool,
    verify: bool,
    dry_run: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let (snap_hash, snap) = load_view_snapshot(env, name)?;
    let exec = datboi_exec::Executor::new(&env.store, datboi_exec::ExecConfig::default())?;
    if !dry_run {
        std::fs::create_dir_all(target)
            .with_context(|| format!("creating {}", target.display()))?;
    }

    // Inventory the card: relative path → size. Symlinks are never
    // followed (or deleted through); stale temp files are removed.
    let mut existing: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if target.is_dir() {
        walk_target(
            target,
            &mut String::new(),
            &mut existing,
            &mut dirs,
            dry_run,
        )?;
    }

    let (mut written, mut skipped, mut bytes_written) = (0usize, 0usize, 0u64);
    for row in &snap.rows {
        let up_to_date = existing.remove(&row.path).is_some_and(|size| {
            size == row.size && (!verify || file_matches(target, &row.path, &row.hash))
        });
        if up_to_date {
            skipped += 1;
            continue;
        }
        written += 1;
        bytes_written += row.size;
        if dry_run {
            continue;
        }
        write_row_verified(&exec, env, target, row)?;
    }

    let mut deleted = 0usize;
    if delete {
        for path in existing.keys() {
            if !dry_run {
                std::fs::remove_file(rel_join(target, path))
                    .with_context(|| format!("deleting extraneous {path}"))?;
            }
            deleted += 1;
        }
        if !dry_run {
            // Deepest-first so newly-emptied parents fall too; non-empty
            // dirs just fail the remove and stay.
            dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
            for dir in &dirs {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }

    if json {
        println!(
            "{}",
            json!({
                "view": name,
                "snapshot": snap_hash.to_hex(),
                "written": written,
                "skipped": skipped,
                "deleted": deleted,
                "extraneous": if delete { 0 } else { existing.len() },
                "bytes_written": bytes_written,
                "dry_run": dry_run,
            })
        );
    } else {
        let verb = if dry_run { "would write" } else { "wrote" };
        print!(
            "sync {name} -> {}: {verb} {written} file(s) ({bytes_written} B), {skipped} up to date",
            target.display()
        );
        if delete {
            println!(", {deleted} deleted");
        } else if existing.is_empty() {
            println!();
        } else {
            println!(", {} extraneous (use --delete)", existing.len());
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Mint (or refresh) the view's FAT32 image recipe from its current
/// snapshot (D62): materialize missing inputs, mint, flip the
/// `image/<name>` tag, optionally export bytes.
pub fn view_image(
    mut env: Env,
    name: &str,
    out: Option<&Path>,
    no_obao: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let (snap_hash, snap) = load_view_snapshot(&env, name)?;
    // Image params live on the definition (CBOR keys 8–11) when the
    // view opted in at define time; the command still works without
    // them — the ruled defaults apply.
    let params = datboi_catalog::get_view(&env.db, name)?
        .and_then(|d| d.image)
        .unwrap_or_default();
    let exec = datboi_exec::Executor::new(&env.store, datboi_exec::ExecConfig::default())?;

    let missing = datboi_catalog::missing_inputs(&env.db, &snap)?;
    let materialized = missing.len();
    for hash in &missing {
        exec.materialize(&env.db, hash)
            .with_context(|| format!("materializing image input {hash}"))?;
    }

    let report = datboi_catalog::mint_image(
        &mut env.db,
        &env.store,
        name,
        &snap_hash,
        &snap,
        &params,
        !no_obao,
        now_unix(),
    )?;

    if let Some(dest) = out {
        write_image_file(&exec, &env, &report.image, report.size, dest)?;
    }

    if json {
        println!(
            "{}",
            json!({
                "view": name,
                "snapshot": snap_hash.to_hex(),
                "image": report.image.to_hex(),
                "recipe": report.recipe.to_hex(),
                "size": report.size,
                "rows": report.rows,
                "skeleton_bytes": report.skeleton_bytes,
                "obao_stored": report.obao_stored,
                "materialized_inputs": materialized,
                "exported": out.map(|p| p.display().to_string()),
            })
        );
    } else {
        println!(
            "image {name}: {} ({} B, {} rows, skeleton {} B{})",
            report.image,
            report.size,
            report.rows,
            report.skeleton_bytes,
            if report.obao_stored {
                ", obao stored"
            } else {
                ", no obao (D63 carve-out serving)"
            },
        );
        if materialized > 0 {
            println!("materialized {materialized} input(s) first");
        }
        if let Some(dest) = out {
            println!("exported to {}", dest.display());
        }
    }
    // D62: read-only synthesis; overlays are a future design pass.
    eprintln!(
        "warning: reflashing this image onto a device CLOBBERS on-device saves \
         (writable overlays are not designed yet)"
    );
    Ok(ExitCode::SUCCESS)
}

/// Stream a blob to one destination file through the verified range
/// path: temp file, 8 MiB windows, fsync, rename.
fn write_image_file(
    exec: &datboi_exec::Executor,
    env: &Env,
    hash: &datboi_core::hash::Blake3,
    size: u64,
    dest: &Path,
) -> anyhow::Result<()> {
    const WINDOW: u64 = 8 << 20;
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = {
        let mut name = dest
            .file_name()
            .map(std::ffi::OsString::from)
            .unwrap_or_default();
        name.push(SYNC_TMP_SUFFIX);
        dest.with_file_name(name)
    };
    let mut out =
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let result = (|| -> anyhow::Result<()> {
        use std::io::Write as _;
        let mut off = 0u64;
        while off < size {
            let want = WINDOW.min(size - off);
            let window = exec.serve_range(&env.db, hash, off, want)?;
            anyhow::ensure!(
                window.len() as u64 == want,
                "short read at {off}: {} of {want} bytes",
                window.len()
            );
            out.write_all(&window)?;
            off += want;
        }
        out.sync_all()?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&tmp, dest)
                .with_context(|| format!("publishing {}", dest.display()))?;
            Ok(())
        }
        Err(e) => {
            drop(out);
            let _ = std::fs::remove_file(&tmp);
            Err(e.context(format!("writing image to {}", dest.display())))
        }
    }
}

/// Join a canonical manifest path under the target dir component-wise.
fn rel_join(target: &Path, rel: &str) -> PathBuf {
    let mut out = target.to_path_buf();
    for component in rel.split('/') {
        out.push(component);
    }
    out
}

fn file_matches(target: &Path, rel: &str, hash: &datboi_core::hash::Blake3) -> bool {
    let Ok(file) = std::fs::File::open(rel_join(target, rel)) else {
        return false;
    };
    let mut hasher = blake3::Hasher::new();
    if std::io::copy(&mut std::io::BufReader::new(file), &mut hasher).is_err() {
        return false;
    }
    hasher.finalize().as_bytes() == &hash.0
}

/// Stream one row into place: temp file, verified 8 MiB windows, fsync,
/// rename — a yanked card never sees a half-written file under its
/// final name.
fn write_row_verified(
    exec: &datboi_exec::Executor,
    env: &Env,
    target: &Path,
    row: &datboi_core::viewsnap::ViewRow,
) -> anyhow::Result<()> {
    const WINDOW: u64 = 8 << 20;
    let dest = rel_join(target, &row.path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Opaque non-resident routes re-spill per window; one verified
    // replay first (same policy as the daemon's long streams).
    if row.seek == 2 {
        let resident = env
            .db
            .blob_by_hash(&row.hash)?
            .is_some_and(|b| b.residency == Residency::Resident);
        if !resident {
            exec.materialize(&env.db, &row.hash)?;
        }
    }
    let tmp = {
        let mut name = dest
            .file_name()
            .map(std::ffi::OsString::from)
            .unwrap_or_default();
        name.push(SYNC_TMP_SUFFIX);
        dest.with_file_name(name)
    };
    let mut out =
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let result = (|| -> anyhow::Result<()> {
        use std::io::Write as _;
        let mut off = 0u64;
        while off < row.size {
            let want = WINDOW.min(row.size - off);
            let window = exec.serve_range(&env.db, &row.hash, off, want)?;
            anyhow::ensure!(
                window.len() as u64 == want,
                "short read at {off} of {}: {} of {want} bytes",
                row.path,
                window.len()
            );
            out.write_all(&window)?;
            off += want;
        }
        out.sync_all()?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&tmp, &dest)
                .with_context(|| format!("publishing {}", dest.display()))?;
            Ok(())
        }
        Err(e) => {
            drop(out);
            let _ = std::fs::remove_file(&tmp);
            Err(e.context(format!("writing {}", row.path)))
        }
    }
}

/// Recursive target inventory. `prefix` is the relative path so far
/// ('/'-separated); symlinks are skipped, our temp leftovers removed.
fn walk_target(
    dir: &Path,
    prefix: &mut String,
    files: &mut std::collections::HashMap<String, u64>,
    dirs: &mut Vec<PathBuf>,
    dry_run: bool,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            // Not representable as a manifest path: leave it alone
            // (it can never match a row, and we won't delete blind).
            continue;
        };
        let meta = std::fs::symlink_metadata(entry.path())?;
        let rel_len = prefix.len();
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(name_str);
        if meta.is_symlink() {
            // never write or delete through links
        } else if meta.is_dir() {
            dirs.push(entry.path());
            walk_target(&entry.path(), prefix, files, dirs, dry_run)?;
        } else if name_str.ends_with(SYNC_TMP_SUFFIX) {
            if !dry_run {
                let _ = std::fs::remove_file(entry.path());
            }
        } else {
            files.insert(prefix.clone(), meta.len());
        }
        prefix.truncate(rel_len);
    }
    Ok(())
}

// ---- auth: users, invites, grants, tokens, sessions (D30/D68) ----

/// Human-readable role name (mirrors the server's whoami vocabulary).
fn role_str(role: datboi_index::Role) -> &'static str {
    match role {
        datboi_index::Role::Owner => "owner",
        datboi_index::Role::Friend => "friend",
    }
}

pub fn user_invite(
    env: &Env,
    owner: bool,
    expires_days: u32,
    base_url: Option<&str>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let role = if owner {
        datboi_index::Role::Owner
    } else {
        datboi_index::Role::Friend
    };
    let token = datboi_server::auth::mint_token().context("minting invite token")?;
    let expires_at = now_unix() + i64::from(expires_days) * 24 * 60 * 60;
    env.db.mint_invite(
        &datboi_server::auth::token_hash(&token),
        None,
        role,
        expires_at,
    )?;
    // The token rides in the FRAGMENT: browsers never send fragments,
    // so the raw token stays out of server/proxy logs — the SPA reads
    // location.hash and POSTs it to /v1/auth/invite/accept.
    let base = base_url.map_or_else(
        || {
            format!(
                "http://{}",
                std::env::var("DATBOI_LISTEN").unwrap_or_else(|_| "127.0.0.1:2352".into())
            )
        },
        |b| b.trim_end_matches('/').to_owned(),
    );
    let url = format!("{base}/invite#{token}");
    if json {
        println!(
            "{}",
            json!({
                "url": url,
                "token": token,
                "role": role_str(role),
                "expires_at": expires_at,
            })
        );
    } else {
        println!("{url}");
        println!(
            "single-use {} invite, expires in {expires_days} day(s)",
            role_str(role)
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn user_list(env: &Env, json: bool) -> anyhow::Result<ExitCode> {
    let users = env.db.list_users()?;
    let grants = env.db.all_grants()?;
    let count = |user_id: i64| grants.iter().filter(|(id, _)| *id == user_id).count();
    if json {
        let items: Vec<_> = users
            .iter()
            .map(|u| {
                json!({
                    "username": u.username,
                    "role": role_str(u.role),
                    "created_at": u.created_at,
                    "grants": count(u.user_id),
                })
            })
            .collect();
        println!("{}", json!({"users": items}));
    } else if users.is_empty() {
        println!("no users (mint an invite: datboi user invite)");
    } else {
        for u in &users {
            println!(
                "{}  {}  {} grant(s)",
                u.username,
                role_str(u.role),
                count(u.user_id)
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn user_grant(
    env: &Env,
    username: &str,
    view: &str,
    grant: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let user = env
        .db
        .user_by_name(username)?
        .with_context(|| format!("no such user {username:?}"))?;
    if grant {
        // Grants may precede the view (they're just names), but a typo
        // is the likelier story — warn, don't refuse.
        if !datboi_catalog::list_views(&env.db)?
            .iter()
            .any(|v| v == view)
        {
            eprintln!("warning: no view named {view:?} is defined (grant recorded anyway)");
        }
        env.db.grant_view(user.user_id, view)?;
    } else if !env.db.revoke_view(user.user_id, view)? {
        eprintln!("note: {username} had no grant for {view:?}");
    }
    if json {
        println!(
            "{}",
            json!({
                "username": username,
                "view": view,
                "granted": grant,
                "grants": env.db.grants_for_user(user.user_id)?,
            })
        );
    } else {
        println!(
            "{} {} {view:?} (now: {})",
            if grant { "granted" } else { "revoked" },
            username,
            env.db.grants_for_user(user.user_id)?.join(", ")
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn token_mint(
    env: &Env,
    username: &str,
    expires_days: u32,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let user = env
        .db
        .user_by_name(username)?
        .with_context(|| format!("no such user {username:?}"))?;
    let token = datboi_server::auth::mint_token().context("minting session token")?;
    let expires_at = now_unix() + i64::from(expires_days) * 24 * 60 * 60;
    env.db.create_session(
        &datboi_server::auth::token_hash(&token),
        user.user_id,
        expires_at,
    )?;
    if json {
        println!(
            "{}",
            json!({"token": token, "username": username, "expires_at": expires_at})
        );
    } else {
        // Printed exactly once — only blake3(token) survives on disk.
        println!("{token}");
        println!(
            "send as `Authorization: Bearer <token>`; acts as {username}, expires in {expires_days} day(s)"
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn session_list(env: &Env, json: bool) -> anyhow::Result<ExitCode> {
    let sessions = env.db.list_sessions()?;
    if json {
        let items: Vec<_> = sessions
            .iter()
            .map(|s| json!({"username": s.username, "expires_at": s.expires_at}))
            .collect();
        println!("{}", json!({"sessions": items}));
    } else if sessions.is_empty() {
        println!("no sessions");
    } else {
        for s in &sessions {
            println!("{}  expires_at {}", s.username, s.expires_at);
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn session_revoke(env: &Env, username: &str, json: bool) -> anyhow::Result<ExitCode> {
    let user = env
        .db
        .user_by_name(username)?
        .with_context(|| format!("no such user {username:?}"))?;
    let revoked = env.db.delete_sessions_for_user(user.user_id)?;
    if json {
        println!("{}", json!({"username": username, "revoked": revoked}));
    } else {
        println!("revoked {revoked} session(s) for {username}");
    }
    Ok(ExitCode::SUCCESS)
}
