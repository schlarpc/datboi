//! M6 iroh spike (docs/p2p.md).
//!
//! Iteration 1 proves the two things the whole M6 plan rests on, with a
//! leaf crate that touches no datboi internals:
//!
//! 1. **Two instances exchange a verified blob.** Stock iroh-blobs over
//!    QUIC, blake3-verified streaming — the transport datboi will front.
//! 2. **iroh's outboard IS our outboard.** iroh serves blake3 bao trees
//!    at a 16 KiB chunk group (`.obao4`); D52 froze our `.obao4` sidecar
//!    at exactly that. The alignment test computes an outboard the way
//!    our store does and checks it against the byte-for-byte golden the
//!    store test committed — so "reuse the obao" (D14/D52) is proven,
//!    not asserted.
//!
//! What this deliberately does NOT do yet: front the real CAS. iroh-blobs
//! 0.103 dropped the custom-store trait (the store is a concrete actor
//! API), so serving our sharded loose-file store means our own
//! `ProtocolHandler` reusing iroh-blobs' bao protocol + get client. That
//! seam, plus set-reconciliation for dedup-aware partial transfer, is the
//! design doc (docs/p2p.md) and the next iteration.

#![allow(clippy::missing_errors_doc)]

use anyhow::Result;
use iroh::{Endpoint, endpoint::presets, protocol::Router};
use iroh_blobs::{BlobsProtocol, store::mem::MemStore, ticket::BlobTicket};

/// A running provider: an endpoint serving one blob store over the blobs
/// ALPN. Holds the router so it stays alive for the connection.
pub struct Provider {
    pub router: Router,
    pub store: MemStore,
}

impl Provider {
    /// Bind an endpoint, put `bytes` into a fresh in-memory store, and
    /// start serving the blobs protocol. Returns the provider and a
    /// ticket a peer can fetch from.
    pub async fn serve(bytes: Vec<u8>) -> Result<(Self, BlobTicket)> {
        let endpoint = Endpoint::bind(presets::N0).await?;
        let store = MemStore::new();
        let tag = store.add_slice(&bytes).await?;
        endpoint.online().await;
        let addr = endpoint.addr();
        let ticket = BlobTicket::new(addr, tag.hash, tag.format);
        let blobs = BlobsProtocol::new(&store, None);
        let router = Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, blobs)
            .spawn();
        Ok((Self { router, store }, ticket))
    }
}

/// A second instance: bind a fresh endpoint + empty store, connect to the
/// provider named in `ticket`, fetch the blob, and return its bytes. The
/// fetch is blake3-verified by iroh-blobs against the ticket's hash.
pub async fn fetch(ticket: &BlobTicket) -> Result<Vec<u8>> {
    let endpoint = Endpoint::bind(presets::N0).await?;
    let store = MemStore::new();
    let conn = endpoint
        .connect(ticket.addr().clone(), iroh_blobs::ALPN)
        .await?;
    store.remote().fetch(conn, ticket.hash()).await?;
    let bytes = store.get_bytes(ticket.hash()).await?;
    Ok(bytes.to_vec())
}

/// A running p2p seedbox: an iroh endpoint serving our holdings (the whole
/// logical CAS, D92/D97) to peers over the blobs protocol. Held by the
/// daemon for the process lifetime; `shutdown` closes it gracefully.
pub struct Seedbox {
    router: Router,
    /// The iroh EndpointId peers reach us by and put on ACLs (D8), display
    /// form — logged at startup so the operator can share it.
    pub node_id: String,
}

impl Seedbox {
    /// The iroh EndpointId (display form).
    #[must_use]
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Close the endpoint and its accept tasks.
    pub async fn shutdown(self) -> Result<()> {
        self.router.shutdown().await?;
        Ok(())
    }
}

/// Bind an iroh endpoint under the **derived** iroh identity (D99 —
/// `identity.iroh_secret()`, never the root or the snapshot key) and serve
/// the datboi logical CAS to peers. The daemon's one entry point: it hands
/// over its `&'static Store` and a dedicated read-only `Db`, and gets back
/// a handle. Returns once bound; serving continues on the router's tasks.
///
/// # Errors
/// Endpoint bind failure (e.g. no network for n0 discovery — the caller
/// logs and runs without p2p rather than aborting the daemon).
pub async fn serve_holdings(
    store: &'static datboi_store_fs::Store,
    db: std::sync::Arc<std::sync::Mutex<datboi_index::Db>>,
    iroh_secret: [u8; 32],
) -> Result<Seedbox> {
    let secret = iroh::SecretKey::from_bytes(&iroh_secret);
    let node_id = secret.public().to_string();
    let provider = cas::CasProvider::new(store, db);
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .bind()
        .await?;
    let router = Router::builder(endpoint)
        .accept(iroh_blobs::ALPN, provider)
        .spawn();
    Ok(Seedbox { router, node_id })
}

/// Fronting the real CAS (D97): serve iroh-blobs' get protocol straight
/// from the datboi CAS, reusing the on-disk `.obao4` sidecar as the bao
/// tree — no custom-store trait exists in iroh-blobs 0.103, so we answer
/// the wire protocol ourselves and iroh-blobs stays the requester.
///
/// This serves the **logical CAS** (D92), not just resident literals.
/// Every request goes through `Executor::serve_range`, which handles both
/// halves uniformly and D49-verified:
///
/// - **Literal** blobs (resident) read from `Store::get` — loose files and
///   D91 packed windows fall through transparently;
/// - **Virtual** blobs (grounded-but-evicted, recipe-only) materialize on
///   demand through the recipe, verified against the `.obao4` that D49 rule
///   1 kept past eviction.
///
/// A peer never learns our residency state — the wire surface is the audit
/// surface. Spike shortcut still standing: the whole range is buffered
/// (`serve_range(0, total)`), so 4 GB ROMs are not yet bounded-memory; the
/// async/streaming bao encoder over `open_stream` is the owed refinement.
pub mod cas {
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};
    use bao_tree::BaoTree;
    use bao_tree::io::outboard::PreOrderMemOutboard;
    use bao_tree::io::sync::encode_ranges_validated;
    use datboi_core::hash::Blake3;
    use datboi_exec::{ExecConfig, Executor};
    use datboi_index::Db;
    use datboi_store_fs::obao::{BLOCK_SIZE, outboard_size};
    use datboi_store_fs::{Namespace, Store};
    use iroh::endpoint::{Connection, SendStream};
    use iroh::protocol::{AcceptError, ProtocolHandler};
    use iroh_blobs::protocol::{GetRequest, Request};

    /// An iroh protocol handler that serves the datboi logical CAS.
    ///
    /// The store is `&'static` — the daemon leaks one `Store` for process
    /// lifetime (its `Executor` borrows it), and this handler shares that
    /// exact instance so it sees packs/evictions as they happen. `Db` wraps
    /// rusqlite and is `!Sync`, so it rides a `Mutex` (a dedicated
    /// read-only handle in the daemon); the guard is dropped before any
    /// await, never held across the wire write.
    #[derive(Clone)]
    pub struct CasProvider {
        store: &'static Store,
        db: Arc<Mutex<Db>>,
    }

    // ProtocolHandler requires Debug; neither a whole CAS nor a DB handle
    // is worth printing — the handler has no state to show.
    impl std::fmt::Debug for CasProvider {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("CasProvider").finish_non_exhaustive()
        }
    }

    impl CasProvider {
        #[must_use]
        pub fn new(store: &'static Store, db: Arc<Mutex<Db>>) -> Self {
            Self { store, db }
        }

        /// Answer one get-request: `size (8 LE) ‖ bao-encoded ranges`, the
        /// exact wire shape iroh-blobs' `export_bao` produces (same
        /// bao-tree 0.16, same 16 KiB block). The bytes come from the
        /// executor's `serve_range` — resident or materialized-from-recipe,
        /// always verified against the output `.obao4` — so a corrupt local
        /// blob or a lying recipe fails here rather than shipping bad bytes.
        fn encode_get(&self, exec: &Executor, get: &GetRequest) -> Result<Option<Vec<u8>>> {
            let hash = Blake3(*get.hash.as_bytes());
            let db = self.db.lock().unwrap_or_else(|e| e.into_inner());

            // Size: a resident literal answers from the store; an evicted
            // blob only the index knows. Unknown to both ⇒ we don't have it.
            let total = match self.store.len(Namespace::Data, &hash)? {
                Some(len) => len,
                None => match db.blob_by_hash(&hash)? {
                    Some(row) => row
                        .size
                        .ok_or_else(|| anyhow!("blob {hash} grounded but size unknown"))?,
                    None => return Ok(None),
                },
            };

            // The whole verified blob (spike buffer). serve_range unifies
            // literal reads and recipe materialization, D49-verified.
            let data = exec.serve_range(&db, &hash, 0, total)?;
            drop(db); // release before the async write; nothing below needs it

            // Outboard: empty for ≤ one chunk group (no sidecar exists);
            // otherwise the retained `.obao4` (survives eviction, D49).
            let sidecar = if outboard_size(total) == 0 {
                Vec::new()
            } else {
                self.store
                    .get_obao(Namespace::Data, &hash)?
                    .ok_or_else(|| anyhow!("no outboard for {hash}"))?
            };
            let outboard = PreOrderMemOutboard {
                root: blake3::Hash::from_bytes(hash.0),
                tree: BaoTree::new(total, BLOCK_SIZE),
                data: sidecar,
            };

            // Root ranges (offset 0); hash-seqs (offset > 0) are later.
            let ranges = get
                .ranges
                .iter_non_empty_infinite()
                .next()
                .map(|(_, r)| r.clone())
                .unwrap_or_else(bao_tree::ChunkRanges::all);

            let mut out = total.to_le_bytes().to_vec();
            encode_ranges_validated(data.as_slice(), &outboard, &ranges, &mut out)?;
            Ok(Some(out))
        }

        async fn serve(
            &self,
            exec: &Executor<'_>,
            get: GetRequest,
            send: &mut SendStream,
        ) -> Result<()> {
            if let Some(bytes) = self.encode_get(exec, &get)? {
                send.write_all(&bytes).await?;
            }
            send.finish()?;
            Ok(())
        }
    }

    impl ProtocolHandler for CasProvider {
        async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
            // One executor per connection (holds the wasm hosts); the store
            // borrow lives for the accept loop. Per-request is the seam if
            // this ever needs a shared engine.
            let exec = Executor::new(self.store, ExecConfig::default())
                .map_err(|e| AcceptError::from_err(std::io::Error::other(e.to_string())))?;
            // One get-request per bidirectional stream, mirroring
            // iroh-blobs' own provider loop.
            while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                let (request, _read) = Request::read_async(&mut recv)
                    .await
                    .map_err(AcceptError::from_err)?;
                if let Request::Get(get) = request {
                    // anyhow isn't std::error::Error; funnel through io::Error.
                    self.serve(&exec, get, &mut send)
                        .await
                        .map_err(|e| AcceptError::from_err(std::io::Error::other(e.to_string())))?;
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headline M6 deliverable: two independent instances, a blob
    /// leaves one and arrives verified at the other. Multi-group size so
    /// the bao tree is exercised, not a single-chunk trivial case.
    #[tokio::test]
    async fn two_instances_exchange_a_verified_blob() -> Result<()> {
        let original: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let (provider, ticket) = Provider::serve(original.clone()).await?;
        let received = fetch(&ticket).await?;
        assert_eq!(received, original, "bytes survived the round trip");
        provider.router.shutdown().await?;
        Ok(())
    }

    /// D97 fronting, LITERAL half: a provider backed by a REAL on-disk
    /// `datboi-store-fs::Store` (loose blob + its `.obao4` sidecar) serves
    /// the stock iroh-blobs requester, which fetches and blake3-verifies.
    /// No bytes are copied into an iroh store — they stream from our CAS
    /// through the executor's `serve_range`, encoded against the sidecar.
    #[tokio::test]
    async fn provider_fronts_a_resident_blob() -> Result<()> {
        use std::sync::{Arc, Mutex};

        use datboi_core::hash::Blake3;
        use datboi_index::{Db, Namespace as IndexNs, Residency};
        use datboi_store_fs::{Namespace, Store};

        let dir = tempfile::tempdir()?;
        // Leak like the daemon (its Executor borrows the store for process
        // lifetime), so the handler can hold `&'static Store`.
        let store: &'static Store = Box::leak(Box::new(Store::open(dir.path().join("store"))?));
        let mut db = Db::open(dir.path())?;
        let original: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let hash = Blake3::compute(&original);
        store.put(Namespace::Data, hash, original.as_slice())?;
        store.ensure_obao(Namespace::Data, &hash)?; // the sidecar we serve from
        db.upsert_blob(
            &hash,
            Some(original.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )?;
        let db = Arc::new(Mutex::new(db));

        let endpoint = Endpoint::bind(presets::N0).await?;
        endpoint.online().await;
        let addr = endpoint.addr();
        let provider = cas::CasProvider::new(store, db);
        let router = Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, provider)
            .spawn();

        let iroh_hash = iroh_blobs::Hash::from_bytes(hash.0);
        let ticket = BlobTicket::new(addr, iroh_hash, iroh_blobs::BlobFormat::Raw);
        let received = fetch(&ticket).await?;
        assert_eq!(received, original, "bytes came verified from the real CAS");

        router.shutdown().await?;
        Ok(())
    }

    /// D97 fronting, VIRTUAL half (D92): a blob that is grounded but NOT
    /// resident — its literal was evicted, only a recipe + the retained
    /// `.obao4` remain — is served to a peer by materializing it on demand
    /// through the executor, D49-verified. The peer can't tell it wasn't
    /// sitting on disk. Fixture mirrors the exec crate's eviction test:
    /// member = `deflate-decompress` of a resident container.
    #[tokio::test]
    async fn provider_fronts_a_virtual_evicted_blob() -> Result<()> {
        use std::io::Write as _;
        use std::sync::{Arc, Mutex};

        use datboi_core::cbor::{self, Value};
        use datboi_core::hash::Blake3;
        use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe};
        use datboi_exec::evict::EvictOutcome;
        use datboi_exec::{ExecConfig, Executor};
        use datboi_index::recipes::NewRecipe;
        use datboi_index::{
            Db, Namespace as IndexNs, OpKind, RecipeSource, Residency, SeekClass,
        };
        use datboi_store_fs::{Namespace, Store};
        use flate2::Compression;
        use flate2::write::DeflateEncoder;

        let dir = tempfile::tempdir()?;
        let store: &'static Store = Box::leak(Box::new(Store::open(dir.path().join("store"))?));
        let mut db = Db::open(dir.path())?;

        // member (>16 KiB so it has a real bao tree); container = "hdr" +
        // deflate(member), so member = deflate-decompress(container[3..]).
        let member: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let member_hash = Blake3::compute(&member);
        let compressed = {
            let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
            enc.write_all(&member)?;
            enc.finish()?
        };
        let mut container = b"hdr".to_vec();
        container.extend_from_slice(&compressed);
        let container_hash = Blake3::compute(&container);

        store.put(Namespace::Data, container_hash, container.as_slice())?;
        let container_id = db.upsert_blob(
            &container_hash,
            Some(container.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )?;
        let member_id = db.upsert_blob(
            &member_hash,
            Some(member.len() as u64),
            IndexNs::Data,
            Residency::Absent,
        )?;
        let recipe = Recipe {
            op: Op::Builtin {
                name: "deflate-decompress".into(),
                major: 1,
            },
            inputs: vec![InputRef {
                hash: container_hash,
                role: None,
            }],
            outputs: vec![OutputRef {
                hash: member_hash,
                size: member.len() as u64,
                name: None,
            }],
            params: cbor::encode(&Value::Map(vec![
                (1, Value::Uint(3)),
                (2, Value::Uint(compressed.len() as u64)),
            ]))?,
        };
        let encoded = recipe.encode()?;
        let recipe_hash = Blake3::compute(&encoded);
        store.put(Namespace::Meta, recipe_hash, encoded.as_slice())?;
        let recipe_blob_id = db.upsert_blob(
            &recipe_hash,
            Some(encoded.len() as u64),
            IndexNs::Meta,
            Residency::Resident,
        )?;
        db.insert_recipe(&NewRecipe {
            blob_id: recipe_blob_id,
            op_kind: OpKind::Builtin,
            op_name: "deflate-decompress@1",
            seek_class: SeekClass::Opaque,
            source: RecipeSource::LocalIngest,
            inputs: &[(0, container_id, None)],
            outputs: &[(0, member_id, member.len() as u64, None)],
        })?;

        // Materialize the member (mints its `.obao4`), then evict the
        // literal — leaving it grounded-but-virtual.
        {
            let exec = Executor::new(store, ExecConfig::default())?;
            exec.materialize(&db, &member_hash)?;
            assert!(store.has(Namespace::Data, &member_hash), "materialized");
            let out = exec.evict(&db, &member_hash)?;
            assert!(matches!(out, EvictOutcome::Evicted { .. }), "evicted: {out:?}");
        }
        assert!(
            !store.has(Namespace::Data, &member_hash),
            "literal gone — the blob is virtual now"
        );
        assert!(
            store.get_obao(Namespace::Data, &member_hash)?.is_some(),
            "outboard retained (D49 rule 1)"
        );

        // Serve the VIRTUAL member to a peer; it must arrive verified,
        // rebuilt from its recipe on the fly.
        let db = Arc::new(Mutex::new(db));
        let endpoint = Endpoint::bind(presets::N0).await?;
        endpoint.online().await;
        let addr = endpoint.addr();
        let provider = cas::CasProvider::new(store, db);
        let router = Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, provider)
            .spawn();

        let ticket = BlobTicket::new(
            addr,
            iroh_blobs::Hash::from_bytes(member_hash.0),
            iroh_blobs::BlobFormat::Raw,
        );
        let received = fetch(&ticket).await?;
        assert_eq!(
            received, member,
            "evicted blob rebuilt from its recipe, verified, over the wire"
        );

        router.shutdown().await?;
        Ok(())
    }

    /// D52 alignment: an outboard built the way our store builds it
    /// (headerless pre-order, 16 KiB chunk groups) over the golden input
    /// must equal the byte-for-byte outboard the store test committed.
    /// Ties iroh's serving format to our frozen at-rest format.
    #[test]
    fn our_obao_format_is_irohs() {
        use bao_tree::io::outboard::PreOrderMemOutboard;
        use bao_tree::io::sync::outboard as fill_outboard;
        use bao_tree::{BaoTree, BlockSize};

        // 16 KiB chunk groups: BlockSize(4) == iroh's IROH_BLOCK_SIZE.
        let block = BlockSize::from_chunk_log(4);
        let data: Vec<u8> = (0..64 * 1024usize).map(|i| (i % 251) as u8).collect();
        let tree = BaoTree::new(data.len() as u64, block);
        let mut ob = PreOrderMemOutboard {
            root: blake3::Hash::from_bytes([0u8; 32]),
            tree,
            data: vec![0u8; tree.outboard_size() as usize],
        };
        let root = fill_outboard(data.as_slice(), tree, &mut ob).expect("outboard");

        assert_eq!(root.as_bytes(), blake3::hash(&data).as_bytes());
        assert_eq!(ob.data.len(), 3 * 64, "4 groups → 3 parent nodes");
        // The D52 golden committed in datboi-store-fs::obao::golden_sidecar.
        assert_eq!(
            blake3::hash(&ob.data).to_hex().as_str(),
            "64f044a9c89de90220352e20f54a47ab6037866f1b8307d84b5b9cacb426f6cd",
        );
    }
}
