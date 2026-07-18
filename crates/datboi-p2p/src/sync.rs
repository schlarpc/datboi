//! The reconcile → fetch-diff → rebuild flow (D100): dedup-aware
//! transfer over the D91 pieces.
//!
//! "Reconcile the plans, fetch the parts": the initiator reconciles its
//! affine-recipe set against the peer's ([`crate::recon`]), fetches the
//! recipe objects it lacks over the blobs ALPN (small, content-verified,
//! indexed as `source=Peer` born `Pending` — the D4/D8 lazy-verify
//! posture), then a LOCAL closure walk names the missing grounding
//! leaves, which fetch as ordinary bao blobs and import with the house
//! discipline (D98: iroh's store owns in-flight, `put_with_obao` owns
//! durable). Explicit wants then materialize through the executor —
//! replay verifies the peer's claim, advancing `Pending → ReplayedLocal`
//! or poisoning a lie (never bad bytes).
//!
//! An empty want-list is mirror mode (D34 full-mirror subscriber
//! policy): every container the fetched plans describe is made grounded
//! — leaves fetched, nothing materialized (residency stays the
//! planner's knob, D91) — and the peer's plan-less content arrives via
//! the D102 roots scope (underived resident literals reconciled
//! alongside the plans), so "everything they share" is complete by
//! construction, not just for decomposed content.
//!
//! Savings are a first-class result (D97): the [`SyncReport`] carries
//! what was fetched vs what was rebuilt vs what was already held, and
//! the completion event emits the same numbers as named numeric tracing
//! fields (D81, OTEL-liftable).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow};
use datboi_core::hash::Blake3;
use datboi_core::recipe::{Op, Recipe};
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Namespace as IndexNs, RecipeSource, Residency, SeekClass, VerifyState};
use datboi_store_fs::{Namespace, Store};
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr};
use iroh_blobs::store::mem::MemStore;

use crate::recon::{self, Scope};

/// What one sync moved, held back, and rebuilt — the D97 savings
/// summary, user-facing shape ("1.3 MiB fetched, 62.7 MiB rebuilt from
/// shared pieces — 98% saved").
#[derive(Debug)]
pub struct SyncReport {
    /// Recipe objects fetched and indexed from the peer.
    pub recipes_fetched: u64,
    pub recipe_bytes_fetched: u64,
    /// Grounding leaves fetched (the D91 pieces we lacked).
    pub pieces_fetched: u64,
    pub piece_bytes_fetched: u64,
    /// Leaves the plans needed that we already held — the dedup win.
    pub pieces_already_held: u64,
    pub bytes_already_held: u64,
    /// Wants materialized locally, with their sizes.
    pub rebuilt: Vec<(Blake3, u64)>,
    pub bytes_rebuilt: u64,
    /// Mirror-mode leaves this peer could not serve (another peer's
    /// plan, or content the peer dropped since advertising). They stay
    /// ungrounded and the next mirror retries them — a count here is
    /// deferral, never loss.
    pub pieces_unavailable: u64,
    /// Reconciliation overhead (header + coded symbols on the wire).
    pub sketch_wire_bytes: u64,
    pub symbols_received: u64,
}

impl SyncReport {
    /// Total bytes pulled off the wire (sketch + plans + parts).
    #[must_use]
    pub fn bytes_fetched(&self) -> u64 {
        self.sketch_wire_bytes + self.recipe_bytes_fetched + self.piece_bytes_fetched
    }

    /// Percent of the rebuilt bytes that never crossed the wire.
    #[must_use]
    pub fn savings_pct(&self) -> f64 {
        if self.bytes_rebuilt == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            100.0 * (1.0 - self.bytes_fetched() as f64 / self.bytes_rebuilt as f64)
        }
    }
}

/// Fetch one blob's bytes from the peer via the D98 staging store
/// (iroh owns in-flight; the caller imports into the CAS on success).
/// The fetch is blake3-verified against `hash` by iroh-blobs itself.
async fn fetch_bytes(staging: &MemStore, conn: &Connection, hash: &Blake3) -> Result<Vec<u8>> {
    let iroh_hash = iroh_blobs::Hash::from_bytes(hash.0);
    staging.remote().fetch(conn.clone(), iroh_hash).await?;
    Ok(staging.get_bytes(iroh_hash).await?.to_vec())
}

/// How one walked blob resolved: both variants are SUPPORT for the
/// blobs above it — `Fetch` because the leaf is committed to the fetch
/// list, so it will be resident before anything assembles from it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Resident, or a route resolved (all inputs Supported/Fetch).
    Supported,
    /// No usable acyclic route: fetch the blob itself. The whole-blob
    /// fallback is this same case, not a special path.
    Fetch,
}

/// The local closure walk: from each root, descend through usable
/// (non-Failed) routes to the grounding leaves, splitting them into
/// already-held and to-fetch.
///
/// CYCLE-CORRECT (D100 use-case audit): real decompositions mint plans
/// in BOTH directions — container = assemble(pieces) and piece =
/// assemble(container[range]) — so a naive visited-set descent would
/// complete the inverse pair with nothing marked missing and ground
/// nothing (D21 forbids exactly that circular support). Here a route
/// whose input is on the CURRENT PATH is unusable for that descent —
/// rooted at a container, the pieces' slice routes point back up, fail,
/// and the pieces resolve `Fetch` (the parts); rooted at a piece, the
/// container's rebuild route fails at the on-path piece and the
/// CONTAINER resolves `Fetch` (slice locally). Memoized outcomes are
/// globally valid: `Fetch` is on the fetch list; `Supported` was proven
/// by a route whose inputs had already resolved off-path.
struct Walk<'a> {
    db: &'a Db,
    memo: HashMap<Blake3, Outcome>,
    path: HashSet<Blake3>,
    missing: Vec<Blake3>,
    pieces_held: u64,
    bytes_held: u64,
}

impl Walk<'_> {
    fn walk(&mut self, hash: &Blake3) -> Result<Outcome> {
        if let Some(outcome) = self.memo.get(hash) {
            return Ok(*outcome);
        }
        let outcome = self.resolve(hash)?;
        self.memo.insert(*hash, outcome);
        if outcome == Outcome::Fetch {
            self.missing.push(*hash);
        }
        Ok(outcome)
    }

    fn resolve(&mut self, hash: &Blake3) -> Result<Outcome> {
        let Some(row) = self.db.blob_by_hash(hash)? else {
            return Ok(Outcome::Fetch);
        };
        if row.residency == Residency::Resident {
            self.pieces_held += 1;
            self.bytes_held += row.size.unwrap_or(0);
            return Ok(Outcome::Supported);
        }
        self.path.insert(*hash);
        let supported = self.any_route_resolves(row.blob_id);
        self.path.remove(hash);
        Ok(if supported? {
            Outcome::Supported
        } else {
            Outcome::Fetch
        })
    }

    fn any_route_resolves(&mut self, blob_id: i64) -> Result<bool> {
        let routes = self.db.recipes_for_output(blob_id)?;
        'routes: for route in routes.iter().filter(|r| r.verify != VerifyState::Failed) {
            let inputs = self.db.recipe_inputs(route.recipe_id)?;
            for input in &inputs {
                if self.path.contains(&input.hash) {
                    continue 'routes; // circular support — D21's forbidden shape
                }
            }
            for input in &inputs {
                self.walk(&input.hash)?; // Supported or Fetch: both support
            }
            return Ok(true);
        }
        Ok(false)
    }
}

/// Reconcile with `peer`, fetch the diff, and ground (and, for explicit
/// `wants`, rebuild) the containers the fetched plans describe. See the
/// module docs for the flow; `wants = []` is mirror mode.
///
/// The `db` handle must be a WRITE handle (recipe indexing, blob
/// upserts, verify-state advances); `store` is the daemon's leaked
/// store, as everywhere in this crate.
pub async fn sync(
    endpoint: &Endpoint,
    peer: EndpointAddr,
    store: &'static Store,
    db: Arc<Mutex<Db>>,
    wants: &[Blake3],
) -> Result<SyncReport> {
    // 1. Reconcile plans (the sketch: our set never crosses the wire).
    //    Mirror mode also reconciles the D102 roots scope — the peer's
    //    underived literals, the content no plan will ever reach — so
    //    "everything they share" is complete by construction. Our prior
    //    is our OWN roots; a peer root we hold as a non-root shows up
    //    in the diff but the walk below resolves it Supported and
    //    fetches nothing (the walk is the dedup filter, D102).
    let (prior, roots_prior) = {
        let guard = db.lock().unwrap_or_else(|e| e.into_inner());
        let prior = guard.affine_recipe_objects()?;
        let roots_prior = if wants.is_empty() {
            guard.root_blobs()?
        } else {
            Vec::new()
        };
        (prior, roots_prior)
    };
    let recon_conn = endpoint
        .connect(peer.clone(), recon::ALPN)
        .await
        .context("connect recon")?;
    let recon = recon::reconcile(&recon_conn, Scope::AffineRecipes, &prior).await?;
    let peer_roots = if wants.is_empty() {
        Some(recon::reconcile(&recon_conn, Scope::RootBlobs, &roots_prior).await?)
    } else {
        None
    };

    // 2. Fetch the plans we lack; verify by content, index as Peer claims.
    let blobs_conn = endpoint
        .connect(peer.clone(), iroh_blobs::ALPN)
        .await
        .context("connect blobs")?;
    let staging = MemStore::new();
    let mut recipes_fetched = 0u64;
    let mut recipe_bytes_fetched = 0u64;
    for hash in &recon.remote_only {
        let bytes = fetch_bytes(&staging, &blobs_conn, hash)
            .await
            .with_context(|| format!("fetch recipe {hash}"))?;
        let Ok(recipe) = Recipe::decode(&bytes) else {
            tracing::warn!(%hash, "peer advertised a non-recipe object; skipped");
            continue;
        };
        let affine = matches!(&recipe.op, Op::Builtin { name, major: 1 } if name == "assemble");
        if !affine {
            tracing::warn!(%hash, op = %recipe.op.index_name(), "peer recipe outside the v1 scope; skipped");
            continue;
        }
        {
            let mut guard = db.lock().unwrap_or_else(|e| e.into_inner());
            store.put(Namespace::Meta, *hash, bytes.as_slice())?;
            let blob_id = guard.upsert_blob(
                hash,
                Some(bytes.len() as u64),
                IndexNs::Meta,
                Residency::Resident,
            )?;
            if guard.recipe_id_for_blob(blob_id)?.is_none() {
                guard.index_recipe(blob_id, &recipe, SeekClass::Affine, RecipeSource::Peer)?;
            }
        }
        recipes_fetched += 1;
        recipe_bytes_fetched += bytes.len() as u64;
        tracing::debug!(%hash, outputs = recipe.outputs.len() as u64, "peer plan indexed");
    }

    // 3. Name the parts: wants drive the walk; mirror mode roots on
    //    EVERY peer-sourced plan output — not just this round's fetches
    //    — so a sync interrupted between plan-indexing and piece-fetch
    //    is finished by the next run instead of orphaned behind an
    //    empty recon diff (D100 use-case audit: the resume gap), PLUS
    //    the peer's remote-only roots (D102): plan-less content —
    //    never-analyzed loose ROMs, preflate-refused containers — walks
    //    as an unknown blob and resolves Fetch. The walk is idempotent,
    //    so re-rooting settled plans (or roots we can already derive)
    //    costs index reads and honestly recounts leaves as already-held.
    let (missing, pieces_already_held, bytes_already_held) = {
        let guard = db.lock().unwrap_or_else(|e| e.into_inner());
        let roots: Vec<Blake3> = if wants.is_empty() {
            let mut roots = guard.peer_plan_outputs()?;
            if let Some(r) = &peer_roots {
                roots.extend(r.remote_only.iter().copied());
            }
            roots
        } else {
            wants.to_vec()
        };
        let mut walk = Walk {
            db: &guard,
            memo: HashMap::new(),
            path: HashSet::new(),
            missing: Vec::new(),
            pieces_held: 0,
            bytes_held: 0,
        };
        for root in &roots {
            walk.walk(root)?;
        }
        (walk.missing, walk.pieces_held, walk.bytes_held)
    };

    // 4. Fetch the missing leaves; import with the house discipline.
    //    Mirror mode tolerates a leaf this peer can't serve (another
    //    peer's plan among the resume roots, or content dropped since
    //    advertising): warn + count, leave it ungrounded for a later
    //    sync. An explicit want is a promise, so wants mode stays
    //    fatal.
    let mut pieces_fetched = 0u64;
    let mut piece_bytes_fetched = 0u64;
    let mut pieces_unavailable = 0u64;
    for hash in &missing {
        let bytes = match fetch_bytes(&staging, &blobs_conn, hash).await {
            Ok(bytes) => bytes,
            Err(e) if wants.is_empty() => {
                tracing::warn!(
                    %hash,
                    error = format!("{e:#}"),
                    "mirror leaf unavailable from this peer; deferred to a later sync"
                );
                pieces_unavailable += 1;
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("fetch piece {hash}")),
        };
        {
            let guard = db.lock().unwrap_or_else(|e| e.into_inner());
            store.put_with_obao(Namespace::Data, *hash, bytes.len() as u64, bytes.as_slice())?;
            guard.upsert_blob(
                hash,
                Some(bytes.len() as u64),
                IndexNs::Data,
                Residency::Resident,
            )?;
        }
        pieces_fetched += 1;
        piece_bytes_fetched += bytes.len() as u64;
        tracing::debug!(%hash, bytes = bytes.len() as u64, "piece imported");
    }

    // 5. Rebuild explicit wants through the executor (replay verifies
    //    the peer's claim: Pending → ReplayedLocal, or poison).
    let mut rebuilt: Vec<(Blake3, u64)> = Vec::new();
    if !wants.is_empty() {
        let db_blocking = Arc::clone(&db);
        let wants_owned = wants.to_vec();
        rebuilt = tokio::task::spawn_blocking(move || -> Result<Vec<(Blake3, u64)>> {
            let exec = Executor::new(store, ExecConfig::default())?;
            let guard = db_blocking.lock().unwrap_or_else(|e| e.into_inner());
            let mut out = Vec::with_capacity(wants_owned.len());
            for want in wants_owned {
                exec.materialize(&guard, &want)
                    .with_context(|| format!("rebuild {want}"))?;
                let size = store
                    .len(Namespace::Data, &want)?
                    .ok_or_else(|| anyhow!("{want} vanished after materialize"))?;
                out.push((want, size));
            }
            Ok(out)
        })
        .await??;
    }
    let bytes_rebuilt = rebuilt.iter().map(|(_, s)| s).sum();

    let report = SyncReport {
        recipes_fetched,
        recipe_bytes_fetched,
        pieces_fetched,
        piece_bytes_fetched,
        pieces_already_held,
        bytes_already_held,
        rebuilt,
        bytes_rebuilt,
        pieces_unavailable,
        // Both sketches (plans + D102 roots) count against the savings.
        sketch_wire_bytes: recon.wire_bytes()
            + peer_roots
                .as_ref()
                .map_or(0, recon::ReconReport::wire_bytes),
        symbols_received: recon.symbols_received
            + peer_roots.as_ref().map_or(0, |r| r.symbols_received),
    };
    // D97: the savings ARE the result. Named numeric fields at the INFO
    // job boundary (D81), OTEL-liftable as-is.
    tracing::info!(
        peer = ?peer.id,
        wants = wants.len() as u64,
        recipes_fetched = report.recipes_fetched,
        recipe_bytes_fetched = report.recipe_bytes_fetched,
        pieces_fetched = report.pieces_fetched,
        piece_bytes_fetched = report.piece_bytes_fetched,
        pieces_already_held = report.pieces_already_held,
        bytes_already_held = report.bytes_already_held,
        containers_rebuilt = report.rebuilt.len() as u64,
        bytes_rebuilt = report.bytes_rebuilt,
        pieces_unavailable = report.pieces_unavailable,
        sketch_wire_bytes = report.sketch_wire_bytes,
        symbols_received = report.symbols_received,
        bytes_fetched = report.bytes_fetched(),
        savings_pct = report.savings_pct(),
        "sync complete"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use datboi_core::assemble::{AssembleParams, Segment};
    use datboi_core::recipe::{InputRef, OutputRef};
    use iroh::endpoint::presets;
    use iroh::protocol::Router;

    use super::*;
    use crate::recon::ReconProvider;

    fn piece(i: u64) -> Vec<u8> {
        // > 16 KiB so pieces carry real bao trees.
        (0..40_960u64)
            .map(|j| ((i * 131 + j) % 251) as u8)
            .collect()
    }

    /// Mint one affine assemble plan (pieces concatenated in order) the
    /// way decomposition does: recipe object into meta, rows into the
    /// index — deterministic, so both instances mint identical bytes for
    /// the shared container (the reconciliation overlap).
    fn mint_assemble(
        db: &mut Db,
        store: &Store,
        pieces: &[(Blake3, u64)],
        container: &Blake3,
        total: u64,
        source: RecipeSource,
    ) -> Result<Blake3> {
        let params = AssembleParams {
            segments: pieces
                .iter()
                .enumerate()
                .map(|(i, (_, len))| Segment::BlobRange {
                    input_ix: u32::try_from(i).expect("few pieces"),
                    offset: 0,
                    len: *len,
                })
                .collect(),
        };
        let recipe = Recipe {
            op: Op::Builtin {
                name: "assemble".into(),
                major: 1,
            },
            inputs: pieces
                .iter()
                .map(|(hash, _)| InputRef {
                    hash: *hash,
                    role: None,
                })
                .collect(),
            outputs: vec![OutputRef {
                hash: *container,
                size: total,
                name: None,
            }],
            params: params.encode()?,
        };
        let bytes = recipe.encode()?;
        let recipe_hash = Blake3::compute(&bytes);
        store.put(Namespace::Meta, recipe_hash, bytes.as_slice())?;
        let blob_id = db.upsert_blob(
            &recipe_hash,
            Some(bytes.len() as u64),
            IndexNs::Meta,
            Residency::Resident,
        )?;
        if db.recipe_id_for_blob(blob_id)?.is_none() {
            db.index_recipe(blob_id, &recipe, SeekClass::Affine, source)?;
        }
        Ok(recipe_hash)
    }

    /// Mint the INVERSE direction real decompositions also mint: one
    /// affine slice plan per piece (`piece = assemble(container[range])`,
    /// the mint_decomposition shape) — together with the rebuild plan
    /// this is the mutually-inverse pair D21 forbids as circular
    /// support, and the shape the cycle-correct walk exists for.
    fn mint_slices(
        db: &mut Db,
        store: &Store,
        container: &Blake3,
        pieces: &[(Blake3, u64)],
        source: RecipeSource,
    ) -> Result<()> {
        let mut offset = 0u64;
        for (hash, len) in pieces {
            let recipe = Recipe {
                op: Op::Builtin {
                    name: "assemble".into(),
                    major: 1,
                },
                inputs: vec![InputRef {
                    hash: *container,
                    role: None,
                }],
                outputs: vec![OutputRef {
                    hash: *hash,
                    size: *len,
                    name: None,
                }],
                params: AssembleParams {
                    segments: vec![Segment::BlobRange {
                        input_ix: 0,
                        offset,
                        len: *len,
                    }],
                }
                .encode()?,
            };
            let bytes = recipe.encode()?;
            let recipe_hash = Blake3::compute(&bytes);
            store.put(Namespace::Meta, recipe_hash, bytes.as_slice())?;
            let blob_id = db.upsert_blob(
                &recipe_hash,
                Some(bytes.len() as u64),
                IndexNs::Meta,
                Residency::Resident,
            )?;
            if db.recipe_id_for_blob(blob_id)?.is_none() {
                db.index_recipe(blob_id, &recipe, SeekClass::Affine, source)?;
            }
            offset += len;
        }
        Ok(())
    }

    fn import_piece(db: &mut Db, store: &Store, bytes: &[u8]) -> Result<(Blake3, u64)> {
        let hash = Blake3::compute(bytes);
        store.put_with_obao(Namespace::Data, hash, bytes.len() as u64, bytes)?;
        db.upsert_blob(
            &hash,
            Some(bytes.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )?;
        Ok((hash, bytes.len() as u64))
    }

    /// The D100 headline, end-to-end over real QUIC: A holds two variant
    /// containers decomposed (6 shared pieces + 2 unique each); B holds
    /// variant 1. B wants variant 2 — and only the plan and the 2 pieces
    /// B lacks cross the wire; the other 6 come from B's own store. The
    /// rebuilt container is byte-true and the peer's claim ends verified
    /// (ReplayedLocal).
    #[tokio::test]
    async fn sync_fetches_only_the_missing_pieces_and_rebuilds() -> Result<()> {
        let shared: Vec<Vec<u8>> = (0..6).map(piece).collect();
        let extra1: Vec<Vec<u8>> = (6..8).map(piece).collect();
        let extra2: Vec<Vec<u8>> = (100..102).map(piece).collect();
        let c1: Vec<u8> = shared.iter().chain(&extra1).flatten().copied().collect();
        let c2: Vec<u8> = shared.iter().chain(&extra2).flatten().copied().collect();
        let c1_hash = Blake3::compute(&c1);
        let c2_hash = Blake3::compute(&c2);

        // Instance A: every piece resident, both plans indexed. The
        // containers themselves are NOT resident — A serves pieces and
        // plans, exactly the decomposed steady state.
        let dir_a = tempfile::tempdir()?;
        let store_a: &'static Store = Box::leak(Box::new(Store::open(dir_a.path().join("s"))?));
        let mut db_a = Db::open(dir_a.path())?;
        let mut a_c1_pieces = Vec::new();
        for p in shared.iter().chain(&extra1) {
            a_c1_pieces.push(import_piece(&mut db_a, store_a, p)?);
        }
        let mut a_c2_pieces = Vec::new();
        for p in shared.iter().chain(&extra2) {
            a_c2_pieces.push(import_piece(&mut db_a, store_a, p)?);
        }
        mint_assemble(
            &mut db_a,
            store_a,
            &a_c1_pieces,
            &c1_hash,
            c1.len() as u64,
            RecipeSource::LocalIngest,
        )?;
        let rc2 = mint_assemble(
            &mut db_a,
            store_a,
            &a_c2_pieces,
            &c2_hash,
            c2.len() as u64,
            RecipeSource::LocalIngest,
        )?;
        let db_a = Arc::new(Mutex::new(db_a));

        let endpoint_a = Endpoint::bind(presets::N0).await?;
        endpoint_a.online().await;
        let addr_a = endpoint_a.addr();
        let router = Router::builder(endpoint_a)
            .accept(
                iroh_blobs::ALPN,
                crate::cas::CasProvider::new(store_a, Arc::clone(&db_a)),
            )
            .accept(recon::ALPN, ReconProvider::new(db_a))
            .spawn();

        // Instance B: variant 1 decomposed (all 8 of its pieces + its
        // plan); variant 2 completely unknown.
        let dir_b = tempfile::tempdir()?;
        let store_b: &'static Store = Box::leak(Box::new(Store::open(dir_b.path().join("s"))?));
        let mut db_b = Db::open(dir_b.path())?;
        let mut b_c1_pieces = Vec::new();
        for p in shared.iter().chain(&extra1) {
            b_c1_pieces.push(import_piece(&mut db_b, store_b, p)?);
        }
        mint_assemble(
            &mut db_b,
            store_b,
            &b_c1_pieces,
            &c1_hash,
            c1.len() as u64,
            RecipeSource::LocalIngest,
        )?;
        let db_b = Arc::new(Mutex::new(db_b));

        let endpoint_b = Endpoint::bind(presets::N0).await?;
        let report = sync(&endpoint_b, addr_a, store_b, Arc::clone(&db_b), &[c2_hash]).await?;

        assert_eq!(report.recipes_fetched, 1, "one plan crossed the wire");
        assert_eq!(report.pieces_fetched, 2, "only the pieces B lacked");
        assert_eq!(report.piece_bytes_fetched, 2 * 40_960);
        assert_eq!(
            report.pieces_already_held, 6,
            "the shared pieces stayed home"
        );
        assert_eq!(report.bytes_already_held, 6 * 40_960);
        assert_eq!(report.bytes_rebuilt, c2.len() as u64);
        assert!(
            report.savings_pct() > 60.0,
            "dedup must dominate: {:.1}%",
            report.savings_pct()
        );

        // The rebuilt container is byte-true on B's disk.
        let mut blob = store_b
            .get(Namespace::Data, &c2_hash)?
            .expect("variant 2 resident at B");
        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut blob, &mut got)?;
        assert_eq!(got, c2, "rebuilt bytes match the original");

        // The peer's claim verified at rebuild: Pending → ReplayedLocal.
        {
            let guard = db_b.lock().unwrap_or_else(|e| e.into_inner());
            let row = guard.blob_by_hash(&rc2)?.expect("plan indexed at B");
            let recipe_id = guard
                .recipe_id_for_blob(row.blob_id)?
                .expect("recipe row at B");
            let recipe = guard.recipe_by_id(recipe_id)?;
            assert_eq!(recipe.verify, VerifyState::ReplayedLocal);
            assert_eq!(recipe.source, RecipeSource::Peer);
        }

        router.shutdown().await?;
        Ok(())
    }

    /// Mirror mode (empty wants): the fetched plans' containers become
    /// grounded — leaves fetched, nothing materialized (residency stays
    /// the planner's knob).
    #[tokio::test]
    async fn mirror_sync_grounds_without_materializing() -> Result<()> {
        let pieces_bytes: Vec<Vec<u8>> = (0..4).map(piece).collect();
        let container: Vec<u8> = pieces_bytes.iter().flatten().copied().collect();
        let c_hash = Blake3::compute(&container);

        let dir_a = tempfile::tempdir()?;
        let store_a: &'static Store = Box::leak(Box::new(Store::open(dir_a.path().join("s"))?));
        let mut db_a = Db::open(dir_a.path())?;
        let mut a_pieces = Vec::new();
        for p in &pieces_bytes {
            a_pieces.push(import_piece(&mut db_a, store_a, p)?);
        }
        mint_assemble(
            &mut db_a,
            store_a,
            &a_pieces,
            &c_hash,
            container.len() as u64,
            RecipeSource::LocalIngest,
        )?;
        let db_a = Arc::new(Mutex::new(db_a));

        let endpoint_a = Endpoint::bind(presets::N0).await?;
        endpoint_a.online().await;
        let addr_a = endpoint_a.addr();
        let router = Router::builder(endpoint_a)
            .accept(
                iroh_blobs::ALPN,
                crate::cas::CasProvider::new(store_a, Arc::clone(&db_a)),
            )
            .accept(recon::ALPN, ReconProvider::new(db_a))
            .spawn();

        let dir_b = tempfile::tempdir()?;
        let store_b: &'static Store = Box::leak(Box::new(Store::open(dir_b.path().join("s"))?));
        let db_b = Arc::new(Mutex::new(Db::open(dir_b.path())?));

        let endpoint_b = Endpoint::bind(presets::N0).await?;
        let report = sync(&endpoint_b, addr_a, store_b, Arc::clone(&db_b), &[]).await?;

        assert_eq!(report.recipes_fetched, 1);
        assert_eq!(report.pieces_fetched, 4, "all leaves fetched");
        assert!(
            report.rebuilt.is_empty(),
            "mirror mode materializes nothing"
        );
        assert!(
            !store_b.has(Namespace::Data, &c_hash),
            "container stays virtual (grounded, not resident)"
        );
        // But it IS grounded: every input of its (peer) plan is resident.
        for p in &pieces_bytes {
            assert!(store_b.has(Namespace::Data, &Blake3::compute(p)));
        }

        router.shutdown().await?;
        Ok(())
    }

    /// The resume gap (D100 use-case audit): a mirror interrupted
    /// between plan-indexing and piece-fetch leaves the plan LOCAL, so
    /// the next run's recon diff is empty — rooting only on this
    /// round's fetches would report success while the leaves stay
    /// missing forever. Mirror roots on every peer-sourced plan
    /// instead: the re-run fetches the orphaned leaves.
    #[tokio::test]
    async fn interrupted_mirror_resumes_on_the_next_sync() -> Result<()> {
        let pieces_bytes: Vec<Vec<u8>> = (0..4).map(piece).collect();
        let container: Vec<u8> = pieces_bytes.iter().flatten().copied().collect();
        let c_hash = Blake3::compute(&container);

        // A: the seeder — pieces resident, plan indexed.
        let dir_a = tempfile::tempdir()?;
        let store_a: &'static Store = Box::leak(Box::new(Store::open(dir_a.path().join("s"))?));
        let mut db_a = Db::open(dir_a.path())?;
        let mut a_pieces = Vec::new();
        for p in &pieces_bytes {
            a_pieces.push(import_piece(&mut db_a, store_a, p)?);
        }
        mint_assemble(
            &mut db_a,
            store_a,
            &a_pieces,
            &c_hash,
            container.len() as u64,
            RecipeSource::LocalIngest,
        )?;
        let db_a = Arc::new(Mutex::new(db_a));

        let endpoint_a = Endpoint::bind(presets::N0).await?;
        endpoint_a.online().await;
        let addr_a = endpoint_a.addr();
        let router = Router::builder(endpoint_a)
            .accept(
                iroh_blobs::ALPN,
                crate::cas::CasProvider::new(store_a, Arc::clone(&db_a)),
            )
            .accept(recon::ALPN, ReconProvider::new(db_a))
            .spawn();

        // B: the interrupted state — the PEER plan already indexed
        // (exactly what sync step 2 commits), zero pieces on disk.
        let dir_b = tempfile::tempdir()?;
        let store_b: &'static Store = Box::leak(Box::new(Store::open(dir_b.path().join("s"))?));
        let mut db_b = Db::open(dir_b.path())?;
        let b_pieces: Vec<(Blake3, u64)> = pieces_bytes
            .iter()
            .map(|p| (Blake3::compute(p), p.len() as u64))
            .collect();
        mint_assemble(
            &mut db_b,
            store_b,
            &b_pieces,
            &c_hash,
            container.len() as u64,
            RecipeSource::Peer,
        )?;
        let db_b = Arc::new(Mutex::new(db_b));

        let endpoint_b = Endpoint::bind(presets::N0).await?;
        let report = sync(&endpoint_b, addr_a, store_b, Arc::clone(&db_b), &[]).await?;

        assert_eq!(
            report.recipes_fetched, 0,
            "recon diff is empty — the plan is already local"
        );
        assert_eq!(
            report.pieces_fetched, 4,
            "the orphaned leaves fetched anyway"
        );
        assert_eq!(report.pieces_unavailable, 0);
        for p in &pieces_bytes {
            assert!(store_b.has(Namespace::Data, &Blake3::compute(p)));
        }

        router.shutdown().await?;
        Ok(())
    }

    /// The D102 completeness proof, the use-case audit's exact gap: a
    /// mirror must reach the content nothing has decomposed. A holds a
    /// decomposed container (plan + pieces) AND a never-analyzed loose
    /// ROM (resident, plan-less — invisible to the recipes scope). The
    /// roots scope carries the loose ROM across; and a root the
    /// initiator can already DERIVE (decomposed at B, whole at A) rides
    /// the diff but fetches nothing — the walk is the dedup filter.
    #[tokio::test]
    async fn mirror_fetches_plan_less_roots() -> Result<()> {
        let pieces_bytes: Vec<Vec<u8>> = (0..4).map(piece).collect();
        let container: Vec<u8> = pieces_bytes.iter().flatten().copied().collect();
        let c_hash = Blake3::compute(&container);
        let loose = piece(200); // never analyzed anywhere
        let loose_hash = Blake3::compute(&loose);
        // Shared root: held loose on BOTH sides — same prior entry, so
        // it never even reaches the diff.
        let shared = piece(201);
        // Derivable root: A holds it whole and unanalyzed; B holds it
        // DECOMPOSED (pieces + rebuild plan, bytes not resident).
        let x_parts: Vec<Vec<u8>> = (202..204).map(piece).collect();
        let x: Vec<u8> = x_parts.iter().flatten().copied().collect();
        let x_hash = Blake3::compute(&x);

        let dir_a = tempfile::tempdir()?;
        let store_a: &'static Store = Box::leak(Box::new(Store::open(dir_a.path().join("s"))?));
        let mut db_a = Db::open(dir_a.path())?;
        let mut a_pieces = Vec::new();
        for p in &pieces_bytes {
            a_pieces.push(import_piece(&mut db_a, store_a, p)?);
        }
        mint_assemble(
            &mut db_a,
            store_a,
            &a_pieces,
            &c_hash,
            container.len() as u64,
            RecipeSource::LocalIngest,
        )?;
        import_piece(&mut db_a, store_a, &loose)?;
        import_piece(&mut db_a, store_a, &shared)?;
        import_piece(&mut db_a, store_a, &x)?;
        let db_a = Arc::new(Mutex::new(db_a));

        let endpoint_a = Endpoint::bind(presets::N0).await?;
        endpoint_a.online().await;
        let addr_a = endpoint_a.addr();
        let router = Router::builder(endpoint_a)
            .accept(
                iroh_blobs::ALPN,
                crate::cas::CasProvider::new(store_a, Arc::clone(&db_a)),
            )
            .accept(recon::ALPN, ReconProvider::new(db_a))
            .spawn();

        let dir_b = tempfile::tempdir()?;
        let store_b: &'static Store = Box::leak(Box::new(Store::open(dir_b.path().join("s"))?));
        let mut db_b = Db::open(dir_b.path())?;
        import_piece(&mut db_b, store_b, &shared)?;
        let mut b_x_pieces = Vec::new();
        for p in &x_parts {
            b_x_pieces.push(import_piece(&mut db_b, store_b, p)?);
        }
        mint_assemble(
            &mut db_b,
            store_b,
            &b_x_pieces,
            &x_hash,
            x.len() as u64,
            RecipeSource::LocalIngest,
        )?;
        let db_b = Arc::new(Mutex::new(db_b));

        let endpoint_b = Endpoint::bind(presets::N0).await?;
        let report = sync(&endpoint_b, addr_a, store_b, Arc::clone(&db_b), &[]).await?;

        assert_eq!(report.recipes_fetched, 1, "A's container plan");
        // 4 container pieces + the loose ROM; NOT the shared root (same
        // prior both sides) and NOT x (derivable at B).
        assert_eq!(report.pieces_fetched, 5);
        assert_eq!(report.pieces_unavailable, 0);
        assert!(
            store_b.has(Namespace::Data, &loose_hash),
            "the plan-less loose ROM mirrored — the audit's invisibility class"
        );
        assert!(
            !store_b.has(Namespace::Data, &x_hash),
            "a root B can derive is not refetched (the walk filtered it)"
        );
        for p in &pieces_bytes {
            assert!(store_b.has(Namespace::Data, &Blake3::compute(p)));
        }

        router.shutdown().await?;
        Ok(())
    }

    /// Real decompositions mint plans BOTH ways — container =
    /// assemble(pieces) AND piece = assemble(container[range]) — the
    /// mutually-inverse pair D21 forbids as circular support. A naive
    /// visited-set walk completes the circle with nothing marked
    /// missing and mirrors NOTHING; the cycle-correct walk fetches one
    /// side. Proof of grounding is materialization: the container must
    /// rebuild byte-true at B afterward.
    #[tokio::test]
    async fn mirror_grounds_inverse_pair_decompositions() -> Result<()> {
        let pieces_bytes: Vec<Vec<u8>> = (0..4).map(piece).collect();
        let container: Vec<u8> = pieces_bytes.iter().flatten().copied().collect();
        let c_hash = Blake3::compute(&container);

        // A: pieces resident + BOTH plan directions (the
        // mint_decomposition shape).
        let dir_a = tempfile::tempdir()?;
        let store_a: &'static Store = Box::leak(Box::new(Store::open(dir_a.path().join("s"))?));
        let mut db_a = Db::open(dir_a.path())?;
        let mut a_pieces = Vec::new();
        for p in &pieces_bytes {
            a_pieces.push(import_piece(&mut db_a, store_a, p)?);
        }
        mint_assemble(
            &mut db_a,
            store_a,
            &a_pieces,
            &c_hash,
            container.len() as u64,
            RecipeSource::LocalIngest,
        )?;
        mint_slices(
            &mut db_a,
            store_a,
            &c_hash,
            &a_pieces,
            RecipeSource::LocalIngest,
        )?;
        let db_a = Arc::new(Mutex::new(db_a));

        let endpoint_a = Endpoint::bind(presets::N0).await?;
        endpoint_a.online().await;
        let addr_a = endpoint_a.addr();
        let router = Router::builder(endpoint_a)
            .accept(
                iroh_blobs::ALPN,
                crate::cas::CasProvider::new(store_a, Arc::clone(&db_a)),
            )
            .accept(recon::ALPN, ReconProvider::new(db_a))
            .spawn();

        // B: empty; mirror the lot (5 plans: 1 rebuild + 4 slices).
        let dir_b = tempfile::tempdir()?;
        let store_b: &'static Store = Box::leak(Box::new(Store::open(dir_b.path().join("s"))?));
        let db_b = Arc::new(Mutex::new(Db::open(dir_b.path())?));

        let endpoint_b = Endpoint::bind(presets::N0).await?;
        let report = sync(&endpoint_b, addr_a, store_b, Arc::clone(&db_b), &[]).await?;

        assert_eq!(report.recipes_fetched, 5);
        assert_eq!(report.pieces_unavailable, 0);
        assert!(
            report.pieces_fetched > 0,
            "the old visited-set walk fetched NOTHING here"
        );

        // Grounding must be REAL, not circular: the container rebuilds
        // byte-true at B through whichever side the walk fetched.
        let exec = Executor::new(store_b, ExecConfig::default())?;
        {
            let guard = db_b.lock().unwrap_or_else(|e| e.into_inner());
            exec.materialize(&guard, &c_hash)?;
        }
        let mut blob = store_b
            .get(Namespace::Data, &c_hash)?
            .expect("container materialized at B");
        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut blob, &mut got)?;
        assert_eq!(got, container, "rebuilt bytes match the original");

        router.shutdown().await?;
        Ok(())
    }
}
