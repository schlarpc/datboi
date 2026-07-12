//! D75 auto-cadence semantics: dirtiness is content-derived from the
//! authoritative triple — first snapshot owed, clean state mints
//! nothing, and any intent change (config keep-mark, tag flip) mints
//! exactly one more.

use datboi_catalog::statesnap;
use datboi_core::hash::Blake3;
use datboi_index::Db;
use datboi_store_fs::Store;

#[test]
fn maybe_mint_fires_on_authoritative_change_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db = Db::open(dir.path()).expect("db");
    let identity = statesnap::load_or_create_identity(dir.path()).expect("identity");

    // Never snapshotted: the first mint is owed unconditionally.
    let first = statesnap::maybe_mint(&store, &db, &identity, 100)
        .expect("mint")
        .expect("first snapshot owed");
    assert_eq!(first.sequence, 1);

    // Clean: nothing moved, nothing mints — the check is idempotent.
    assert!(
        statesnap::maybe_mint(&store, &db, &identity, 200)
            .expect("check")
            .is_none(),
        "clean state must not churn snapshots"
    );

    // Operator intent lands (a D73 keep-mark is a config row): the
    // next ambient tick owes a snapshot.
    db.config_set("gc:keep:cafe", b"1").expect("config");
    let second = statesnap::maybe_mint(&store, &db, &identity, 300)
        .expect("mint")
        .expect("config change owes a snapshot");
    assert_eq!(second.sequence, 2);
    assert!(
        statesnap::maybe_mint(&store, &db, &identity, 400)
            .expect("check")
            .is_none()
    );

    // A tag flip (view eval, image mint) is authoritative too.
    db.set_tag("view/gba", &Blake3::compute(b"snap"), 500)
        .expect("tag");
    let third = statesnap::maybe_mint(&store, &db, &identity, 600)
        .expect("mint")
        .expect("tag change owes a snapshot");
    assert_eq!(third.sequence, 3);

    // The minted object round-trips under our key (what recovery
    // will do with it).
    use std::io::Read as _;
    let mut bytes = Vec::new();
    store
        .get(datboi_store_fs::Namespace::Meta, &third.hash)
        .expect("get")
        .expect("stored")
        .read_to_end(&mut bytes)
        .expect("read");
    let snap = datboi_core::snapshot::StateSnapshot::decode(&bytes).expect("decode");
    snap.verify(&identity.public_key()).expect("our signature");
    assert!(
        snap.payload
            .config
            .iter()
            .any(|(key, _)| key == "gc:keep:cafe"),
        "keep-mark rode the snapshot"
    );
}
