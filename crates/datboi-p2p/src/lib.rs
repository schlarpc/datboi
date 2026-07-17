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
