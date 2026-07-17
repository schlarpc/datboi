//! M6 iroh spike (docs/p2p.md).
//!
//! Iteration 1 proves the two things the whole M6 plan rests on, with a
//! leaf crate that touches no datboi internals:
//!
//! 1. **Two instances exchange a verified blob.** Stock iroh-blobs over
//!    QUIC, blake3-verified streaming — the transport datboi will front.
//! 2. **iroh's outboard IS our outboard.** iroh serves blake3 bao trees
//!    at a 16 KiB chunk group (`.obao4`); D52 froze our `.obao` sidecar
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

/// Fronting the real CAS (D97): serve iroh-blobs' get protocol straight
/// from a `datboi-store-fs::Store`, reusing the on-disk `.obao` sidecar as
/// the bao tree — no custom-store trait exists in iroh-blobs 0.103, so we
/// answer the wire protocol ourselves and iroh-blobs stays the requester.
///
/// Iteration 2 serves LITERAL blobs (loose files + D91 packed windows fall
/// through `Store::get` transparently). The virtual half — grounded-but-
/// evicted blobs materialized through the executor (D92) — slots in at the
/// same seam: produce the bytes some other way, encode the same tree.
pub mod cas {
    use std::sync::Arc;

    use anyhow::Result;
    use bao_tree::BaoTree;
    use bao_tree::io::outboard::PreOrderMemOutboard;
    use bao_tree::io::sync::encode_ranges_validated;
    use datboi_core::hash::Blake3;
    use datboi_store_fs::obao::BLOCK_SIZE;
    use datboi_store_fs::{Namespace, Store};
    use iroh::endpoint::{Connection, SendStream};
    use iroh::protocol::{AcceptError, ProtocolHandler};
    use iroh_blobs::protocol::{GetRequest, Request};

    /// An iroh protocol handler that serves blobs from the datboi CAS.
    #[derive(Clone)]
    pub struct CasProvider {
        store: Arc<Store>,
    }

    // ProtocolHandler requires Debug; the Store isn't Debug (nor should a
    // whole CAS be) — the handler has no state worth printing.
    impl std::fmt::Debug for CasProvider {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("CasProvider").finish_non_exhaustive()
        }
    }

    impl CasProvider {
        #[must_use]
        pub fn new(store: Arc<Store>) -> Self {
            Self { store }
        }

        /// Answer one get-request: `size (8 LE) ‖ bao-encoded ranges`, the
        /// exact wire shape iroh-blobs' `export_bao` produces (same
        /// bao-tree 0.16, same 16 KiB block). The requested chunk ranges
        /// verify against our `.obao` as they encode, so a corrupt local
        /// blob fails the encode rather than shipping bad bytes.
        fn encode_get(&self, get: &GetRequest) -> Result<Option<Vec<u8>>> {
            let hash = Blake3(*get.hash.as_bytes());
            let Some(len) = self.store.len(Namespace::Data, &hash)? else {
                return Ok(None);
            };
            // The root ranges (offset 0). Hash-seqs (offset > 0) are a
            // later iteration; a plain blob only has a root entry.
            let ranges = get
                .ranges
                .iter_non_empty_infinite()
                .next()
                .map(|(_, r)| r.clone())
                .unwrap_or_else(bao_tree::ChunkRanges::all);

            // Whole-blob read is the spike shortcut; streaming/spill is the
            // executor path (D92) the virtual half already implies.
            let mut data = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
            let mut blob = self
                .store
                .get(Namespace::Data, &hash)?
                .expect("len() saw the blob; single-writer tree");
            std::io::Read::read_to_end(&mut blob, &mut data)?;

            let sidecar = self
                .store
                .get_obao(Namespace::Data, &hash)?
                .unwrap_or_default();
            let tree = BaoTree::new(len, BLOCK_SIZE);
            let outboard = PreOrderMemOutboard {
                root: blake3::Hash::from_bytes(hash.0),
                tree,
                data: sidecar,
            };

            let mut out = Vec::new();
            out.extend_from_slice(&len.to_le_bytes());
            encode_ranges_validated(data.as_slice(), &outboard, &ranges, &mut out)?;
            Ok(Some(out))
        }

        async fn serve(&self, get: GetRequest, send: &mut SendStream) -> Result<()> {
            if let Some(bytes) = self.encode_get(&get)? {
                send.write_all(&bytes).await?;
            }
            send.finish()?;
            Ok(())
        }
    }

    impl ProtocolHandler for CasProvider {
        async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
            // One get-request per bidirectional stream, mirroring
            // iroh-blobs' own provider loop.
            while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                let (request, _read) = Request::read_async(&mut recv)
                    .await
                    .map_err(AcceptError::from_err)?;
                if let Request::Get(get) = request {
                    // anyhow isn't std::error::Error; funnel through io::Error.
                    self.serve(get, &mut send)
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

    /// D97 fronting: a provider backed by a REAL on-disk
    /// `datboi-store-fs::Store` (loose blob + its `.obao` sidecar) serves
    /// the stock iroh-blobs requester, which fetches and blake3-verifies.
    /// No bytes are copied into an iroh store — they stream from our CAS,
    /// encoded against the sidecar we already wrote at ingest.
    #[tokio::test]
    async fn provider_fronts_the_real_cas() -> Result<()> {
        use std::sync::Arc;

        use datboi_core::hash::Blake3;
        use datboi_store_fs::{Namespace, Store};

        let dir = tempfile::tempdir()?;
        let store = Arc::new(Store::open(dir.path().join("store"))?);
        let original: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let hash = Blake3::compute(&original);
        store.put(Namespace::Data, hash, original.as_slice())?;
        store.ensure_obao(Namespace::Data, &hash)?; // the sidecar we serve from

        let endpoint = Endpoint::bind(presets::N0).await?;
        endpoint.online().await;
        let addr = endpoint.addr();
        let provider = cas::CasProvider::new(store.clone());
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
