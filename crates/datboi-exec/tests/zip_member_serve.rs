//! The D63-amendment read path: a DEFLATE zip member bigger than one
//! bao group must be readable through [`Executor::serve_range`] — the
//! call every serving surface (`nfs.rs`, `http.rs`, `dav.rs`) funnels
//! into — straight off an ingest, with no replay and no eviction in
//! between.
//!
//! That is the shape `tests/gate.rs` misses: it REPLAYS the member
//! first, which materializes it and builds the outboard as a side
//! effect, so the never-blessed case never ran there. In the field
//! nothing replays, a `deflate-decompress@1` route is not affine so
//! the D63 carve-out declines it, and every member over 16 KiB
//! answered `MissingOutboard` — HTTP 500, `NFS3ERR_IO`, an empty read
//! for the client. Members at or under one group have an empty
//! outboard by construction and kept working, which is what hid it.

use std::fs;

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Residency, SeekClass, VerifyState};
use datboi_ingest::Ingester;
use datboi_store_fs::{Namespace as StoreNs, Store, layout, obao};
use flate2::Compression;
use flate2::write::DeflateEncoder;

/// 18-and-a-bit bao groups: enough interior tree that an off-by-one in
/// the window math would show, and comfortably past the 16 KiB
/// threshold that made the bug invisible.
const BIG_LEN: usize = 300_000;

/// Deterministic, DEFLATE-friendly: a pseudo-random 1 KiB tile salted
/// per 64 KiB, so the stream compresses but is not one repeated block.
fn pattern(len: usize, salt: u8) -> Vec<u8> {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D ^ u64::from(salt);
    let tile: Vec<u8> = (0..1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect();
    (0..len)
        .map(|i| tile[i % 1024] ^ ((i >> 16) as u8) ^ salt)
        .collect()
}

#[derive(Default)]
struct ZipBuilder {
    data: Vec<u8>,
    central: Vec<u8>,
    entries: u16,
}

impl ZipBuilder {
    fn add(&mut self, name: &str, contents: &[u8], deflate: bool) -> &mut Self {
        let (method, payload) = if deflate {
            let mut enc = DeflateEncoder::new(Vec::new(), Compression::fast());
            std::io::Write::write_all(&mut enc, contents).expect("deflate");
            (8u16, enc.finish().expect("deflate finish"))
        } else {
            (0u16, contents.to_vec())
        };
        let mut crc = crc32fast::Hasher::new();
        crc.update(contents);
        let crc = crc.finalize();
        let local_offset = u32::try_from(self.data.len()).expect("small fixture");
        let comp = u32::try_from(payload.len()).expect("small fixture");
        let uncomp = u32::try_from(contents.len()).expect("small fixture");
        let name_len = u16::try_from(name.len()).expect("short name");

        self.data.extend_from_slice(b"PK\x03\x04");
        self.data.extend_from_slice(&20u16.to_le_bytes());
        self.data.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.data.extend_from_slice(&method.to_le_bytes());
        self.data.extend_from_slice(&[0; 4]); // dos time+date
        self.data.extend_from_slice(&crc.to_le_bytes());
        self.data.extend_from_slice(&comp.to_le_bytes());
        self.data.extend_from_slice(&uncomp.to_le_bytes());
        self.data.extend_from_slice(&name_len.to_le_bytes());
        self.data.extend_from_slice(&0u16.to_le_bytes()); // extra len
        self.data.extend_from_slice(name.as_bytes());
        self.data.extend_from_slice(&payload);

        self.central.extend_from_slice(b"PK\x01\x02");
        self.central.extend_from_slice(&20u16.to_le_bytes());
        self.central.extend_from_slice(&20u16.to_le_bytes());
        self.central.extend_from_slice(&0u16.to_le_bytes());
        self.central.extend_from_slice(&method.to_le_bytes());
        self.central.extend_from_slice(&[0; 4]);
        self.central.extend_from_slice(&crc.to_le_bytes());
        self.central.extend_from_slice(&comp.to_le_bytes());
        self.central.extend_from_slice(&uncomp.to_le_bytes());
        self.central.extend_from_slice(&name_len.to_le_bytes());
        self.central.extend_from_slice(&[0; 2]); // extra
        self.central.extend_from_slice(&[0; 2]); // comment
        self.central.extend_from_slice(&[0; 2]); // disk start
        self.central.extend_from_slice(&[0; 2]); // internal attrs
        self.central.extend_from_slice(&[0; 4]); // external attrs
        self.central.extend_from_slice(&local_offset.to_le_bytes());
        self.central.extend_from_slice(name.as_bytes());

        self.entries += 1;
        self
    }

    fn finish(&self) -> Vec<u8> {
        let mut out = self.data.clone();
        let cd_offset = u32::try_from(out.len()).expect("small fixture");
        out.extend_from_slice(&self.central);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&self.entries.to_le_bytes());
        out.extend_from_slice(&self.entries.to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(self.central.len())
                .expect("small")
                .to_le_bytes(),
        );
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&[0; 2]);
        out
    }
}

struct World {
    dir: tempfile::TempDir,
    store: Store,
    db: Db,
    /// `deflate` (BIG_LEN, deflated), `stored` (BIG_LEN, stored),
    /// `tiny` (under one group, deflated).
    deflate: Vec<u8>,
    stored: Vec<u8>,
    tiny: Vec<u8>,
}

impl World {
    fn hash(bytes: &[u8]) -> Blake3 {
        Blake3::compute(bytes)
    }

    fn exec(&self) -> Executor<'_> {
        Executor::new(
            &self.store,
            ExecConfig {
                spill_dir: Some(self.dir.path().to_owned()),
                ..ExecConfig::default()
            },
        )
        .expect("executor")
    }

    fn sidecar_path(&self, hash: &Blake3) -> std::path::PathBuf {
        self.dir
            .path()
            .join("store")
            .join(layout::outboard_path(StoreNs::Data, hash))
    }
}

/// Build the fixture zip and ingest it. Nothing else runs: no replay,
/// no eviction, no blessing pass — the state a freshly-adopted corpus
/// is actually in when a client first reads it.
fn ingest_fixture() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    let deflate = pattern(BIG_LEN, 0x11);
    let stored = pattern(BIG_LEN, 0x22);
    let tiny = pattern(4096, 0x33);
    let mut zb = ZipBuilder::default();
    zb.add("big.rom", &deflate, true)
        .add("raw.rom", &stored, false)
        .add("tiny.rom", &tiny, true);
    let zip_path = dir.path().join("game.zip");
    fs::write(&zip_path, zb.finish()).expect("zip");

    let report = Ingester::new(&store, &mut db, &[]).ingest(&[&zip_path]);
    assert_eq!(report.errors, vec![], "{:?}", report.errors);
    assert_eq!(report.member_skips, vec![], "{:?}", report.member_skips);
    assert_eq!(report.members_claimed, 3);

    World {
        dir,
        store,
        db,
        deflate,
        stored,
        tiny,
    }
}

/// Every offset worth trying on a multi-group blob: the two group
/// boundaries nearest each end, the interior, and a span that ends
/// exactly at EOF.
fn probes(len: u64) -> Vec<(u64, u64)> {
    let g = obao::GROUP_BYTES;
    vec![
        (0, 64),
        (g - 1, 2),
        (g, 64),
        (g + 1, 64),
        (len / 2, 3 * g + 7),
        (len - g - 1, g),
        (len - 1, 1),
        (0, len),
    ]
}

fn assert_serves(exec: &Executor<'_>, db: &Db, hash: &Blake3, want: &[u8], what: &str) {
    for (offset, take) in probes(want.len() as u64) {
        let take = take.min(want.len() as u64 - offset);
        let got = exec
            .serve_range(db, hash, offset, take)
            .unwrap_or_else(|e| panic!("{what}: range {offset}+{take} refused: {e}"));
        let lo = usize::try_from(offset).expect("fits");
        let hi = lo + usize::try_from(take).expect("fits");
        assert_eq!(got, &want[lo..hi], "{what}: wrong bytes at {offset}");
    }
}

/// Fresh off an ingest — the member's bytes are absent, its recipe has
/// never been replayed, and the route is opaque. It must still read.
#[test]
fn fresh_deflate_member_reads_through_the_serve_path() {
    let w = ingest_fixture();
    let hash = World::hash(&w.deflate);

    // Preconditions: this really is the never-blessed, non-affine case.
    let blob_id = w.db.get_blob_id(&hash).expect("q").expect("claimed");
    let row = w.db.blob_by_hash(&hash).expect("q").expect("claimed");
    assert_eq!(row.residency, Residency::Absent, "member bytes are a claim");
    assert!(!w.store.has(StoreNs::Data, &hash));
    let recipes = w.db.recipes_for_output(blob_id).expect("q");
    assert_eq!(recipes.len(), 1);
    assert_eq!(recipes[0].op_name, "deflate-decompress@1");
    assert_eq!(
        recipes[0].seek_class,
        SeekClass::Opaque,
        "no D63 carve-out for a deflate route"
    );
    assert_eq!(
        recipes[0].verify,
        VerifyState::Verified,
        "nothing replayed this"
    );

    let exec = w.exec();
    assert_serves(&exec, &w.db, &hash, &w.deflate, "fresh deflate member");
}

/// The ingest half (D63 amendment A): the outboard is built in the same
/// inflate that builds the alias tuple, so it exists the moment the
/// claim does — and its root is the hash the claim names.
#[test]
fn ingest_leaves_a_tree_for_the_non_affine_member_only() {
    let w = ingest_fixture();

    let big = World::hash(&w.deflate);
    let sidecar = w
        .store
        .get_obao(StoreNs::Data, &big)
        .expect("q")
        .expect("ingest blessed the deflate member");
    assert_eq!(sidecar.len() as u64, obao::outboard_size(BIG_LEN as u64));
    // Same value two ways: the sidecar ingest wrote is bit-identical to
    // one computed from the member bytes, and roots at the claim.
    let (root, expected) = obao::compute(&w.deflate[..], BIG_LEN as u64).expect("compute");
    assert_eq!(root, big, "obao root IS the member's blake3");
    assert_eq!(sidecar, expected);
    // The member itself is still absent — a sidecar with no `.data`
    // beside it is exactly the post-eviction shape D49 rule 1 keeps.
    assert!(!w.store.has(StoreNs::Data, &big));

    // STORED members are affine over the container: the carve-out
    // already serves every byte of them verified, so D63's cost
    // objection stands and ingest writes no tree.
    let raw = World::hash(&w.stored);
    assert_eq!(
        w.store.get_obao(StoreNs::Data, &raw).expect("q"),
        None,
        "no sidecar for a STORED member — the carve-out covers it"
    );

    // Under one group the outboard is empty by construction; absence IS
    // the sidecar, and nothing should have been written.
    let tiny = World::hash(&w.tiny);
    assert_eq!(w.store.get_obao(StoreNs::Data, &tiny).expect("q"), None);
    assert!(!w.sidecar_path(&tiny).exists());
}

/// The carve-out is untouched: a STORED member has no tree and needs
/// none — D63's affine path still serves it.
#[test]
fn stored_member_still_serves_off_the_carve_out() {
    let w = ingest_fixture();
    let hash = World::hash(&w.stored);
    let exec = w.exec();
    assert_serves(&exec, &w.db, &hash, &w.stored, "stored member");
    assert_eq!(
        w.store.get_obao(StoreNs::Data, &hash).expect("q"),
        None,
        "the carve-out serves without minting a tree"
    );
}

/// Small members were always fine (empty outboard by construction) —
/// pin that, since it is the fact that hid the bug for months.
#[test]
fn tiny_deflate_member_reads_with_no_tree_at_all() {
    let w = ingest_fixture();
    let hash = World::hash(&w.tiny);
    let exec = w.exec();
    let got = exec.serve_range(&w.db, &hash, 1000, 512).expect("range");
    assert_eq!(got, &w.tiny[1000..1512]);
}
