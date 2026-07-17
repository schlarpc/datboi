//! The reconciliation protocol (D100): a second `ProtocolHandler` on the
//! datboi ALPN beside the blobs seedbox.
//!
//! Roles are asymmetric by design (the privacy ruling): the RESPONDER
//! streams rateless coded symbols ([`crate::riblt`]) over its scope's
//! set; the INITIATOR decodes against a local prior that never crosses
//! the wire, and reveals only the 1-byte scope request plus a stop
//! signal. The answering party is the consenting party.
//!
//! Wire (fixed binary, D19 register):
//! - initiator → responder: `[scope: u8]`, later `[0u8]` = stop;
//! - responder → initiator: `[set_size: u64 LE]` then 48-byte coded
//!   symbols (batched) until stop, stream closure, or the drain cap.
//!
//! v1's one scope is `AffineRecipes` — the meta-blob hashes of
//! non-Failed builtin `assemble@1` routes. Reconcile the plans; the
//! parts (D91 pieces) follow from a local closure walk (D100:
//! "reconcile the plans, fetch the parts").

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail};
use datboi_core::hash::Blake3;
use datboi_index::Db;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};

use crate::riblt::{self, CODED_SYMBOL_LEN, CodedSymbol, SetSnapshot, Symbol};

/// The recon ALPN. Versioned: an algorithm or wire change is a new ALPN,
/// not an in-band negotiation (D100 keeps the codec swappable this way).
pub const ALPN: &[u8] = b"datboi/recon/1";

/// Responder-side drain cap: enough symbols to decode a symmetric
/// difference approaching ~700k (at the ~1.35×d constant plus slack) —
/// far beyond any v1 corpus — while bounding what one request can pull.
/// Raising it is policy work, not a wire change.
const MAX_SYMBOLS: u64 = 1 << 20;

/// First encode-block size (D100 amendment): one set scan produces this
/// many coded symbols, enough to decode a diff of ~750 — so the common
/// small-diff reconciliation is exactly ONE pass and ~48 KiB of coded
/// state, regardless of corpus size.
const FIRST_BLOCK: usize = 1024;

/// Block-growth ceiling: caps per-block memory at 6 MiB of coded
/// symbols while keeping a full drain to `MAX_SYMBOLS` at O(log) set
/// scans (doubling from `FIRST_BLOCK`, then ~8 max-size blocks).
const MAX_BLOCK: usize = 1 << 17;

/// What a responder will reconcile (the wire's first byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Scope {
    /// Meta-blob hashes of non-Failed affine builtin `assemble@1`
    /// routes ([`Db::affine_recipe_objects`]).
    AffineRecipes = 0,
}

impl Scope {
    fn from_byte(b: u8) -> Option<Self> {
        (b == Scope::AffineRecipes as u8).then_some(Scope::AffineRecipes)
    }
}

/// The responder: streams a scope's coded-symbol sequence until the
/// initiator stops it, encoding off a sqlite SNAPSHOT rather than a
/// resident set (D100 amendment) — each stream opens its own read-only
/// connection, holds one read transaction across every encode pass
/// (WAL snapshot isolation = the [`SetSnapshot`] stability contract),
/// and holds only the current block in memory. Stateless across
/// requests.
#[derive(Clone)]
pub struct ReconProvider {
    /// The databases directory — each stream opens a dedicated
    /// read-only connection here (derived from the daemon's handle at
    /// construction, so callers keep passing the handle they have).
    db_dir: PathBuf,
}

impl std::fmt::Debug for ReconProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReconProvider").finish_non_exhaustive()
    }
}

impl ReconProvider {
    /// The handle is only borrowed long enough to learn where the
    /// databases live; serving opens per-stream read-only connections
    /// (the D100-amendment snapshot shape), never this handle.
    #[must_use]
    pub fn new(db: Arc<Mutex<Db>>) -> Self {
        let guard = db.lock().unwrap_or_else(|e| e.into_inner());
        let db_dir = guard
            .cache_path()
            .parent()
            .expect("cache.db lives in the db dir")
            .to_path_buf();
        Self { db_dir }
    }
}

/// [`SetSnapshot`] over a held-transaction connection: one `for_each`
/// is one pass of the scope query. Distinctness is structural in the
/// query; stability is the transaction the caller holds around every
/// pass ([`stream_scope`]).
struct DbSnapshot<'a> {
    db: &'a Db,
    scope: Scope,
}

impl SetSnapshot for DbSnapshot<'_> {
    type Error = anyhow::Error;

    fn for_each(&mut self, f: &mut dyn FnMut(Symbol)) -> Result<(), Self::Error> {
        match self.scope {
            Scope::AffineRecipes => self.db.for_each_affine_recipe_object(&mut |h| f(h.0))?,
        }
        Ok(())
    }
}

/// The blocking half of one recon stream (D100 amendment): open the
/// dedicated read-only connection, pin one read transaction, and send
/// the size header + exponentially growing coded blocks through the
/// channel until stop/cap/hang-up. A failed send means the async side
/// hung up (initiator stop or wire death) — a normal exit. Returns
/// (advertised set size, symbols sent).
fn stream_scope(
    db_dir: &Path,
    scope: Scope,
    stopped: &AtomicBool,
    tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<(u64, u64)> {
    let db = Db::open_read_only(db_dir).context("recon snapshot connection")?;
    // BEGIN (deferred, read-only): the first read pins the WAL snapshot
    // every subsequent pass sees — writers proceed, this stream's set
    // is frozen. COMMIT (best-effort) releases the pinned WAL segment.
    db.cache().execute_batch("BEGIN")?;
    let result = (|| -> Result<(u64, u64)> {
        let set_size = match scope {
            Scope::AffineRecipes => db.affine_recipe_object_count()?,
        };
        if tx.blocking_send(set_size.to_le_bytes().to_vec()).is_err() {
            return Ok((set_size, 0));
        }
        let mut sent = 0u64;
        let mut block_len = FIRST_BLOCK;
        while sent < MAX_SYMBOLS && !stopped.load(Ordering::Relaxed) {
            let take = block_len.min(usize::try_from(MAX_SYMBOLS - sent).unwrap_or(usize::MAX));
            let mut block = vec![CodedSymbol::default(); take];
            let mut source = DbSnapshot { db: &db, scope };
            riblt::encode_block(&mut source, sent, &mut block)?;
            let mut bytes = Vec::with_capacity(take * CODED_SYMBOL_LEN);
            for c in &block {
                bytes.extend_from_slice(&c.to_bytes());
            }
            if tx.blocking_send(bytes).is_err() {
                break;
            }
            sent += take as u64;
            block_len = (block_len * 2).min(MAX_BLOCK);
        }
        Ok((set_size, sent))
    })();
    let _ = db.cache().execute_batch("COMMIT");
    result
}

impl ProtocolHandler for ReconProvider {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let mut scope_byte = [0u8; 1];
            if recv.read_exact(&mut scope_byte).await.is_err() {
                continue;
            }
            let Some(scope) = Scope::from_byte(scope_byte[0]) else {
                // Unknown scope: refuse the stream, keep the connection
                // (a newer peer probing an older responder).
                continue;
            };

            // The stop watcher: the initiator either sends a stop byte or
            // drops its stream (STOP_SENDING makes our write fail). Both
            // raise the flag the encoder polls between blocks; stop
            // latency is bounded by one block's encode pass.
            let stopped = Arc::new(AtomicBool::new(false));
            let watcher = tokio::spawn({
                let stopped = Arc::clone(&stopped);
                async move {
                    let _ = recv.read_exact(&mut [0u8; 1]).await;
                    stopped.store(true, Ordering::Relaxed);
                }
            });

            // spawn_blocking + bounded channel (the CasProvider bridge
            // shape): sqlite passes happen off the async threads, and a
            // slow wire backpressures the encoder at ≤ 2 blocks in
            // flight.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
            let encode = tokio::task::spawn_blocking({
                let db_dir = self.db_dir.clone();
                let stopped = Arc::clone(&stopped);
                move || stream_scope(&db_dir, scope, &stopped, &tx)
            });

            while let Some(bytes) = rx.recv().await {
                if send.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            // Dropping the receiver fails any pending blocking_send, so
            // a dead wire stops the encoder mid-drain.
            drop(rx);
            stopped.store(true, Ordering::Relaxed);
            watcher.abort();
            match encode.await {
                Ok(Ok((set_size, sent))) => {
                    let _ = send.finish();
                    tracing::debug!(
                        scope = ?scope,
                        set_size,
                        symbols_sent = sent,
                        "recon stream served"
                    );
                }
                Ok(Err(e)) => {
                    return Err(AcceptError::from_err(std::io::Error::other(e.to_string())));
                }
                Err(e) => {
                    return Err(AcceptError::from_err(std::io::Error::other(e.to_string())));
                }
            }
        }
        Ok(())
    }
}

/// One decoded reconciliation, initiator side.
#[derive(Debug)]
pub struct ReconReport {
    pub scope: Scope,
    /// Responder's advertised set size.
    pub remote_set: u64,
    /// Our prior's size.
    pub local_set: u64,
    /// Coded symbols consumed before the decode completed.
    pub symbols_received: u64,
    /// Hashes the responder has that we lack (what to fetch).
    pub remote_only: Vec<Blake3>,
    /// Hashes we have that the responder lacks (never leaves this
    /// process — the D100 asymmetric reveal).
    pub local_only: Vec<Blake3>,
}

impl ReconReport {
    /// Wire bytes spent on the sketch (header + symbols): the cost the
    /// savings telemetry compares against shipping the plain set.
    #[must_use]
    pub fn wire_bytes(&self) -> u64 {
        8 + self.symbols_received * CODED_SYMBOL_LEN as u64
    }
}

/// Reconcile `local` against a peer's scope over an open recon
/// connection: request the scope, feed the decoder our prior, consume
/// coded symbols until the symmetric difference decodes, then stop the
/// responder. Errors on stream failure or on blowing the convergence
/// budget (a responder that can't be decoded — malformed stream or a
/// diff beyond the cap).
pub async fn reconcile(
    conn: &Connection,
    scope: Scope,
    local: &[Blake3],
) -> Result<ReconReport> {
    let (mut send, mut recv) = conn.open_bi().await.context("open recon stream")?;
    send.write_all(&[scope as u8])
        .await
        .context("send recon scope")?;

    let mut header = [0u8; 8];
    recv.read_exact(&mut header).await.context("recon header")?;
    let remote_set = u64::from_le_bytes(header);
    let local_set = local.len() as u64;

    let mut dec = riblt::Decoder::new(local.iter().map(|h| h.0));

    // The difference cannot exceed |A|+|B|, and decoding needs ~1.35×d
    // symbols (higher constants in the small-d regime) — 8× the union
    // plus slack is comfortably past any honest stream.
    let budget = 8 * (remote_set + local_set) + 1024;
    let mut symbols_received = 0u64;
    let mut buf = [0u8; CODED_SYMBOL_LEN];
    loop {
        if symbols_received >= budget {
            bail!(
                "reconciliation did not converge within {budget} symbols \
                 (remote set {remote_set}, local set {local_set})"
            );
        }
        recv.read_exact(&mut buf)
            .await
            .context("recon symbol stream ended before decode")?;
        dec.add_coded_symbol(CodedSymbol::from_bytes(&buf));
        symbols_received += 1;
        dec.try_decode();
        if dec.is_malformed() {
            bail!("peer recon stream is malformed (peeling invariant violated)");
        }
        if dec.decoded() {
            break;
        }
    }
    // Graceful stop; the responder also stops on our stream closing.
    let _ = send.write_all(&[0u8]).await;
    let _ = send.finish();

    let remote_only: Vec<Blake3> = dec.remote().map(|s| Blake3(*s)).collect();
    let local_only: Vec<Blake3> = dec.local().map(|s| Blake3(*s)).collect();
    let report = ReconReport {
        scope,
        remote_set,
        local_set,
        symbols_received,
        remote_only,
        local_only,
    };
    // D97 savings observability: named numeric fields, INFO at the job
    // boundary (D81), so an OTEL layer lifts them into metrics unchanged.
    let diff = (report.remote_only.len() + report.local_only.len()) as u64;
    tracing::info!(
        scope = ?scope,
        local_set,
        remote_set,
        symbols_received,
        diff_remote = report.remote_only.len() as u64,
        diff_local = report.local_only.len() as u64,
        wire_bytes = report.wire_bytes(),
        overhead_ratio = symbols_received as f64 / diff.max(1) as f64,
        "reconcile decoded"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use datboi_index::recipes::NewRecipe;
    use datboi_index::{Namespace as IndexNs, OpKind, RecipeSource, Residency, SeekClass};
    use iroh::protocol::Router;
    use iroh::{Endpoint, endpoint::presets};

    use super::*;

    fn fake_hash(tag: &str, i: u64) -> Blake3 {
        Blake3::compute(format!("{tag}:{i}").as_bytes())
    }

    /// Insert one affine assemble@1 recipe row whose object blob is
    /// `hash` — the minimal shape [`Db::affine_recipe_objects`] selects.
    fn insert_affine_recipe(db: &mut Db, hash: &Blake3, i: u64) -> Result<()> {
        let blob_id = db.upsert_blob(hash, Some(64), IndexNs::Meta, Residency::Resident)?;
        let out_id = db.upsert_blob(
            &fake_hash("container", i),
            Some(1024),
            IndexNs::Data,
            Residency::Absent,
        )?;
        db.insert_recipe(&NewRecipe {
            blob_id,
            op_kind: OpKind::Builtin,
            op_name: "assemble@1",
            seek_class: SeekClass::Affine,
            source: RecipeSource::LocalIngest,
            inputs: &[],
            outputs: &[(0, out_id, 1024, None)],
        })?;
        Ok(())
    }

    /// End-to-end over real QUIC: a responder advertising 30 recipes, an
    /// initiator holding 25 of them plus 5 of its own — the decoded diff
    /// is exactly 5 each way, and the initiator's extras never needed a
    /// wire representation (the asymmetric reveal).
    #[tokio::test]
    async fn reconcile_recovers_the_recipe_diff_over_the_wire() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;
        let responder_set: Vec<Blake3> = (0..30).map(|i| fake_hash("recipe", i)).collect();
        for (i, hash) in responder_set.iter().enumerate() {
            insert_affine_recipe(&mut db, hash, i as u64)?;
        }
        let db = Arc::new(Mutex::new(db));

        let endpoint = Endpoint::bind(presets::N0).await?;
        endpoint.online().await;
        let addr = endpoint.addr();
        let router = Router::builder(endpoint)
            .accept(ALPN, ReconProvider::new(db))
            .spawn();

        // Prior: 25 shared + 5 the responder has never seen.
        let mut local: Vec<Blake3> = responder_set[5..].to_vec();
        local.extend((0..5).map(|i| fake_hash("local-extra", i)));

        let client = Endpoint::bind(presets::N0).await?;
        let conn = client.connect(addr, ALPN).await?;
        let report = reconcile(&conn, Scope::AffineRecipes, &local).await?;

        assert_eq!(report.remote_set, 30);
        assert_eq!(report.local_set, 30);
        let mut remote_only = report.remote_only.clone();
        remote_only.sort_unstable_by_key(|h| h.0);
        let mut want: Vec<Blake3> = responder_set[..5].to_vec();
        want.sort_unstable_by_key(|h| h.0);
        assert_eq!(remote_only, want, "the 5 recipes we lack, exactly");
        let mut local_only = report.local_only.clone();
        local_only.sort_unstable_by_key(|h| h.0);
        let mut want: Vec<Blake3> = (0..5).map(|i| fake_hash("local-extra", i)).collect();
        want.sort_unstable_by_key(|h| h.0);
        assert_eq!(local_only, want, "our 5 extras, decoded locally");

        router.shutdown().await?;
        Ok(())
    }

    /// The D100-amendment stability contract, exercised directly: a
    /// read transaction held across passes sees ONE frozen set while a
    /// writer commits a new recipe mid-stream; a fresh transaction sees
    /// the growth. This is the property `stream_scope` rests on — if
    /// sqlite ever stopped giving it, blocks would encode different
    /// sets and the sketch would be garbage.
    #[test]
    fn held_transaction_freezes_the_scope_across_passes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut writer = Db::open(dir.path())?;
        for i in 0..10 {
            insert_affine_recipe(&mut writer, &fake_hash("recipe", i), i)?;
        }
        let reader = Db::open_read_only(dir.path())?;
        reader.cache().execute_batch("BEGIN")?;
        let count = reader.affine_recipe_object_count()?; // pins the snapshot
        insert_affine_recipe(&mut writer, &fake_hash("recipe", 99), 99)?;
        let mut pass1 = Vec::new();
        reader.for_each_affine_recipe_object(&mut |h| pass1.push(h))?;
        let mut pass2 = Vec::new();
        reader.for_each_affine_recipe_object(&mut |h| pass2.push(h))?;
        reader.cache().execute_batch("COMMIT")?;
        assert_eq!(count, 10);
        assert_eq!(pass1.len(), 10, "mid-stream write invisible to the held snapshot");
        assert_eq!(pass1, pass2, "every pass sees the identical set");
        reader.cache().execute_batch("BEGIN")?;
        let after = reader.affine_recipe_object_count()?;
        reader.cache().execute_batch("COMMIT")?;
        assert_eq!(after, 11, "a fresh transaction sees the commit");
        Ok(())
    }

    /// Identical sets decode from the very first coded symbol.
    #[tokio::test]
    async fn identical_sets_cost_one_symbol() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(dir.path())?;
        let set: Vec<Blake3> = (0..100).map(|i| fake_hash("recipe", i)).collect();
        for (i, hash) in set.iter().enumerate() {
            insert_affine_recipe(&mut db, hash, i as u64)?;
        }
        let db = Arc::new(Mutex::new(db));

        let endpoint = Endpoint::bind(presets::N0).await?;
        endpoint.online().await;
        let addr = endpoint.addr();
        let router = Router::builder(endpoint)
            .accept(ALPN, ReconProvider::new(db))
            .spawn();

        let client = Endpoint::bind(presets::N0).await?;
        let conn = client.connect(addr, ALPN).await?;
        let report = reconcile(&conn, Scope::AffineRecipes, &set).await?;
        assert_eq!(report.symbols_received, 1);
        assert!(report.remote_only.is_empty());
        assert!(report.local_only.is_empty());

        router.shutdown().await?;
        Ok(())
    }
}
