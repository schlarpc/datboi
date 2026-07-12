//! State-snapshot minting + the D75 auto-cadence check, shared by
//! `datboi snapshot` and the daemon's maintenance cycle (one
//! definition; the CLI wrapper only prints).
//!
//! Cadence dirtiness is AUTHORITATIVE-ONLY by design: the check
//! compares (sources, tags, config) against the last snapshot's
//! payload — the fields that carry operator intent (view defs, GC
//! policy, keep-marks, dat lineage). Alias/analysis batches are
//! deliberately NOT part of the trigger: they are derivable
//! cache-grade rows whose loss costs recovery TIME, not truth, and
//! re-encoding every shard per cycle to detect their drift would be
//! the expensive way to learn what `datboi snapshot` already offers.
//! When a mint DOES fire, it refreshes the batches anyway — intent
//! changes carry the freshest recovery aids along for free.
//!
//! Dirtiness is content-derived, never tracked: no dirty flags to
//! forget, no counters to desync — "would the authoritative triple
//! differ from what the log's newest snapshot holds?" A missing or
//! foreign-keyed snapshot object answers dirty (re-mint under our
//! key is the honest response to both).

use std::path::{Path, PathBuf};

use datboi_core::hash::Blake3;
use datboi_core::identity::Identity;
use datboi_core::snapshot::{
    AliasBatch, AnalysisBatch, SnapshotPayload, SourceRef, StateSnapshot, alias_shard,
};
use datboi_index::{Db, Namespace as NsRow, Residency};
use datboi_store_fs::{Namespace, PutOutcome, Store};

use crate::CatalogError;

/// Alias/analysis batch fanout — snapshot encoder policy (the format
/// carries the value; changing it only re-shards future snapshots).
pub const ALIAS_FANOUT: usize = 256;

/// What one mint produced (the CLI prints this; the daemon logs it).
#[derive(Debug)]
pub struct MintReport {
    pub hash: Blake3,
    pub sequence: i64,
    pub sources: usize,
    pub alias_rows: u64,
    pub analysis_rows: u64,
    pub new_batch_blobs: u64,
}

fn identity_path(db_dir: &Path) -> PathBuf {
    db_dir.join("identity.key")
}

/// Load the instance identity if the key file exists.
///
/// # Errors
/// I/O, or a key file of the wrong size (never guessed at).
pub fn load_identity(db_dir: &Path) -> Result<Option<Identity>, CatalogError> {
    let path = identity_path(db_dir);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                CatalogError::Statesnap(format!("{} must be exactly 32 bytes", path.display()))
            })?;
            Ok(Some(Identity::from_seed(seed)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Load the identity, generating and persisting one on first use
/// (0600 — the seed IS the instance identity, D15's recovery root).
///
/// # Errors
/// I/O; key generation.
pub fn load_or_create_identity(db_dir: &Path) -> Result<Identity, CatalogError> {
    if let Some(identity) = load_identity(db_dir)? {
        return Ok(identity);
    }
    let identity = Identity::generate()
        .map_err(|e| CatalogError::Statesnap(format!("generating instance identity: {e}")))?;
    let path = identity_path(db_dir);
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&identity.to_seed())?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, identity.to_seed())?;
    eprintln!(
        "note: generated instance identity at {} — back this file up out-of-band (D15)",
        path.display()
    );
    Ok(identity)
}

/// The (sources, tags, config) intent triple in the payload's own
/// sorted shapes — the D75 comparison unit.
type AuthoritativeTriple = (
    Vec<SourceRef>,
    Vec<(String, Blake3)>,
    Vec<(String, Vec<u8>)>,
);

/// The authoritative triple as the next mint would record it.
fn authoritative_triple(db: &Db) -> Result<AuthoritativeTriple, CatalogError> {
    let sources: Vec<SourceRef> = db
        .list_current_sources()?
        .into_iter()
        .map(|(provider, system, dat_blob, imported_at)| SourceRef {
            provider,
            system,
            dat_blob,
            imported_at: u64::try_from(imported_at).unwrap_or(0),
        })
        .collect();
    let mut tags = db.list_tags()?;
    tags.sort_by(|a, b| a.0.cmp(&b.0));
    let mut config = db.config_list_prefix("")?;
    config.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((sources, tags, config))
}

/// The D75 cadence question: does the authoritative triple differ
/// from the newest logged snapshot's payload?
///
/// # Errors
/// Index/store I/O. Undecodable or foreign-keyed snapshot objects are
/// DIRTY, not errors — re-minting under our key is the fix for both.
pub fn authoritative_dirty(
    store: &Store,
    db: &Db,
    identity: &Identity,
) -> Result<bool, CatalogError> {
    let Some((hash, _seq)) = db.latest_snapshot()? else {
        return Ok(true); // never snapshotted: the first mint is owed
    };
    let Some(mut file) = store.get(Namespace::Meta, &hash)? else {
        return Ok(true); // log points at vanished bytes: re-mint
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    let Ok(snap) = StateSnapshot::decode(&bytes) else {
        return Ok(true);
    };
    if snap.verify(&identity.public_key()).is_err() {
        return Ok(true); // someone else's snapshot: ours is owed
    }
    let (sources, tags, config) = authoritative_triple(db)?;
    Ok(sources != snap.payload.sources
        || tags != snap.payload.tags
        || config != snap.payload.config)
}

/// Mint one snapshot: alias + analysis batches (sharded,
/// content-dedup'd), the authoritative triple inline, signed, stored,
/// logged. The whole body is the former `datboi snapshot`
/// implementation, verbatim in behavior.
///
/// # Errors
/// Index/store I/O; a snapshot-log/sequence race (two concurrent
/// minters — the daemon serializes on its one worker, and the CLI
/// racing it surfaces here loudly rather than mislabeling a
/// sequence).
pub fn mint(
    store: &Store,
    db: &Db,
    identity: &Identity,
    now: i64,
) -> Result<MintReport, CatalogError> {
    let (sources, tags, config) = authoritative_triple(db)?;

    let mut shards: Vec<Vec<datboi_core::alias::AliasTuple>> = vec![Vec::new(); ALIAS_FANOUT];
    let mut alias_rows: u64 = 0;
    for tuple in db.list_alias_tuples()? {
        shards[alias_shard(&tuple.blake3, ALIAS_FANOUT)].push(tuple);
        alias_rows += 1;
    }
    let mut alias_batches = Vec::with_capacity(ALIAS_FANOUT);
    let mut new_batch_blobs: u64 = 0;
    for rows in shards {
        let bytes = AliasBatch { rows }.encode()?;
        let (hash, aliases, outcome) = store.put_new(Namespace::Meta, bytes.as_slice())?;
        if outcome == PutOutcome::Stored {
            new_batch_blobs += 1;
        }
        let blob_id = db.upsert_blob(&hash, Some(aliases.size), NsRow::Meta, Residency::Resident)?;
        db.insert_aliases(blob_id, &aliases)?;
        db.set_verified(blob_id, now)?;
        alias_batches.push(hash);
    }

    let analysis_rows_all = db.list_analysis_rows()?;
    let analysis_rows = analysis_rows_all.len() as u64;
    let mut analysis_batches = Vec::new();
    if !analysis_rows_all.is_empty() {
        let mut shards: Vec<Vec<datboi_core::snapshot::AnalysisRow>> =
            vec![Vec::new(); ALIAS_FANOUT];
        for row in analysis_rows_all {
            shards[alias_shard(&row.blob, ALIAS_FANOUT)].push(row);
        }
        for rows in shards {
            let bytes = AnalysisBatch { rows }.encode()?;
            let (hash, aliases, outcome) = store.put_new(Namespace::Meta, bytes.as_slice())?;
            if outcome == PutOutcome::Stored {
                new_batch_blobs += 1;
            }
            let blob_id =
                db.upsert_blob(&hash, Some(aliases.size), NsRow::Meta, Residency::Resident)?;
            db.insert_aliases(blob_id, &aliases)?;
            db.set_verified(blob_id, now)?;
            analysis_batches.push(hash);
        }
    }

    let sequence = db.next_snapshot_seq()?;
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
        tags,
        config,
    };
    let source_count = payload.sources.len();
    let bytes = payload.encode_signed(identity)?;
    let (hash, aliases, _outcome) = store.put_new(Namespace::Meta, bytes.as_slice())?;
    let blob_id = db.upsert_blob(&hash, Some(aliases.size), NsRow::Meta, Residency::Resident)?;
    db.insert_aliases(blob_id, &aliases)?;
    db.set_verified(blob_id, now)?;
    let logged = db.snapshot_log_append(&hash, now)?;
    if logged != sequence {
        return Err(CatalogError::Statesnap(format!(
            "snapshot_log assigned seq {logged}, object was minted with {sequence} \
             (concurrent snapshot?)"
        )));
    }
    Ok(MintReport {
        hash,
        sequence,
        sources: source_count,
        alias_rows,
        analysis_rows,
        new_batch_blobs,
    })
}

/// The D75 rider: mint iff the authoritative triple moved.
///
/// # Errors
/// See [`authoritative_dirty`] and [`mint`].
pub fn maybe_mint(
    store: &Store,
    db: &Db,
    identity: &Identity,
    now: i64,
) -> Result<Option<MintReport>, CatalogError> {
    if !authoritative_dirty(store, db, identity)? {
        return Ok(None);
    }
    mint(store, db, identity, now).map(Some)
}
