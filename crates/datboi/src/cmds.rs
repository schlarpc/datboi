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
    let payload = SnapshotPayload {
        sequence: u64::try_from(sequence).unwrap_or(0),
        created_at: u64::try_from(now).unwrap_or(0),
        sources,
        alias_fanout: ALIAS_FANOUT,
        alias_batches,
        analysis_fanout,
        analysis_batches,
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

    let report = exec.evict_covered(&env.db, target_bytes, license)?;
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
        "preflate" | "preflate-split" => Box::new(datboi_ingest::analyzers::PreflateZipAnalyzer::new()),
        other => {
            anyhow::bail!("unknown analyzer {other:?} (available: noop, chunk, deflate-trial)")
        }
    };
    let report =
        datboi_ingest::refine::run_sweep(&mut env.db, &env.store, analyzer.as_mut(), limit)?;
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
    if selected.is_some() {
        fast_walk = true;
        for item in env.store.list_parallel(Namespace::Data, RECOVER_WALK_WORKERS) {
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

    {
        {
            if let Some((snap_hash, snap)) = selected {
                // Alias restore (D43, the fast-walk counterpart of the
                // full-read pass): batch rows for blobs the walk found
                // get their alias tuples back without reading bytes.
                // Rows for vanished blobs are skipped; blobs newer than
                // the snapshot get aliases from the next scrub.
                if fast_walk {
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

pub fn scrub(env: &Env, sample_pct: u8, rehabilitate: bool, json: bool) -> anyhow::Result<ExitCode> {
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
