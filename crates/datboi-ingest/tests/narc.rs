//! narc-split interior decomposition over a real store: a synthetic
//! NARC (BTAF/BTNF/GMIF, 4-byte-aligned members with 0xFF padding, one
//! empty member) is decomposed, and — the critical assertion — the
//! minted rebuild recipe executed over piece bytes derived by the minted
//! slice recipes reproduces the archive bit-for-bit. Plus the
//! recipe-volume gate and the not-a-NARC refusal.

use std::io::Read as _;

use datboi_core::assemble::{self, AssembleParams};
use datboi_core::hash::Blake3;
use datboi_core::recipe::Recipe;
use datboi_index::{Db, Namespace as IndexNs, Residency};
use datboi_ingest::analyzers::NarcAnalyzer;
use datboi_ingest::refine::run_sweep;
use datboi_store_fs::{Namespace as StoreNs, Store};

fn world() -> (tempfile::TempDir, Store, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db = Db::open(dir.path()).expect("db");
    (dir, store, db)
}

fn put(store: &Store, db: &Db, bytes: &[u8]) -> (Blake3, i64) {
    let hash = Blake3::compute(bytes);
    store.put(StoreNs::Data, hash, bytes).expect("put");
    let id = db
        .upsert_blob(&hash, Some(bytes.len() as u64), IndexNs::Data, Residency::Resident)
        .expect("row");
    (hash, id)
}

fn sweep(db: &mut Db, store: &Store) -> datboi_ingest::refine::SweepReport {
    let exec =
        datboi_exec::Executor::new(store, datboi_exec::ExecConfig::default()).expect("executor");
    let bytes = datboi_ingest::refine::Logical::new(store, &exec);
    let mut analyzer = NarcAnalyzer;
    run_sweep(db, store, &bytes, &mut analyzer, 100).expect("sweep")
}

fn analysis_details(db: &Db) -> Vec<String> {
    db.cache()
        .prepare("SELECT COALESCE(detail,'') FROM analysis")
        .expect("q")
        .query_map([], |r| r.get(0))
        .expect("q")
        .collect::<Result<_, _>>()
        .expect("q")
}

fn blob_hash_of(db: &Db, blob_id: i64) -> Blake3 {
    let bytes: Vec<u8> = db
        .cache()
        .query_row("SELECT hash FROM blob WHERE blob_id = ?1", (blob_id,), |row| {
            row.get(0)
        })
        .expect("blob row");
    Blake3(bytes.try_into().expect("32 bytes"))
}

fn recipes_for(store: &Store, db: &Db, output: &Blake3) -> Vec<Recipe> {
    let blob_id = db.get_blob_id(output).expect("query").expect("output blob row");
    db.recipes_for_output(blob_id)
        .expect("recipes")
        .iter()
        .map(|row| {
            let recipe_hash = blob_hash_of(db, row.blob_id);
            let mut bytes = Vec::new();
            store
                .get(StoreNs::Meta, &recipe_hash)
                .expect("get")
                .expect("recipe blob resident")
                .read_to_end(&mut bytes)
                .expect("read");
            Recipe::decode(&bytes).expect("valid recipe object")
        })
        .collect()
}

fn materialize(params: &AssembleParams, sources: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    assemble::reader(params, sources)
        .expect("valid")
        .read_to_end(&mut out)
        .expect("materialize");
    out
}

fn derive_piece(store: &Store, db: &Db, narc: &[u8], narc_hash: &Blake3, piece: &Blake3) -> Vec<u8> {
    let recipe = recipes_for(store, db, piece)
        .into_iter()
        .find(|r| r.inputs.len() == 1 && r.inputs[0].hash == *narc_hash)
        .expect("piece has a NARC-derive recipe");
    let params = AssembleParams::decode(&recipe.params).expect("slice params");
    materialize(&params, &[narc])
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

/// Assemble a synthetic NARC: header + BTAF (FAT) + minimal BTNF (FNT) +
/// GMIF (file image), members 0xFF-padded to a 4-byte boundary. Members
/// are the byte ranges the FAT points at, relative to GMIF data.
fn build_narc(members: &[&[u8]]) -> Vec<u8> {
    let mut fat: Vec<(u32, u32)> = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    for m in members {
        let start = data.len() as u32;
        data.extend_from_slice(m);
        fat.push((start, data.len() as u32));
        while !data.len().is_multiple_of(4) {
            data.push(0xFF); // Nitro alignment padding.
        }
    }

    let mut btaf = Vec::new();
    btaf.extend_from_slice(b"BTAF");
    btaf.extend_from_slice(&((12 + fat.len() * 8) as u32).to_le_bytes());
    btaf.extend_from_slice(&(members.len() as u16).to_le_bytes());
    btaf.extend_from_slice(&0u16.to_le_bytes()); // reserved
    for (s, e) in &fat {
        btaf.extend_from_slice(&s.to_le_bytes());
        btaf.extend_from_slice(&e.to_le_bytes());
    }

    // Minimal but real FNT: one root directory, no named files.
    let mut btnf = Vec::new();
    btnf.extend_from_slice(b"BTNF");
    btnf.extend_from_slice(&16u32.to_le_bytes());
    btnf.extend_from_slice(&4u32.to_le_bytes()); // root sub-table offset
    btnf.extend_from_slice(&0u16.to_le_bytes()); // first file id
    btnf.extend_from_slice(&1u16.to_le_bytes()); // total dirs

    let mut gmif = Vec::new();
    gmif.extend_from_slice(b"GMIF");
    gmif.extend_from_slice(&((8 + data.len()) as u32).to_le_bytes());
    gmif.extend_from_slice(&data);

    let total = (0x10 + btaf.len() + btnf.len() + gmif.len()) as u32;
    let mut narc = Vec::new();
    narc.extend_from_slice(b"NARC");
    narc.extend_from_slice(&0xFFFEu16.to_le_bytes());
    narc.extend_from_slice(&0x0100u16.to_le_bytes());
    narc.extend_from_slice(&total.to_le_bytes());
    narc.extend_from_slice(&0x10u16.to_le_bytes());
    narc.extend_from_slice(&3u16.to_le_bytes());
    narc.extend_from_slice(&btaf);
    narc.extend_from_slice(&btnf);
    narc.extend_from_slice(&gmif);
    narc
}

#[test]
fn round_trip_is_bit_faithful_and_members_are_claimed() {
    // Three real members (odd-length first, so it pads) plus an empty one.
    let a = pattern(301, 11);
    let b = pattern(500, 22);
    let c = pattern(200, 33);
    let narc = build_narc(&[&a, &[], &b, &c]);

    let (_dir, store, mut db) = world();
    let (narc_hash, _) = put(&store, &db, &narc);

    let report = sweep(&mut db, &store);
    assert_eq!(report.errors, vec![]);
    assert_eq!(
        (report.positive, report.negative),
        (1, 0),
        "details: {:?}",
        analysis_details(&db)
    );

    // THE round trip: rebuild recipe × derived piece bytes == the NARC.
    let rebuilds = recipes_for(&store, &db, &narc_hash);
    let rebuild = rebuilds
        .iter()
        .find(|r| !r.inputs.is_empty() && r.outputs[0].hash == narc_hash)
        .expect("one rebuild claim for the NARC");
    let params = AssembleParams::decode(&rebuild.params).expect("rebuild params");
    let piece_bytes: Vec<Vec<u8>> = rebuild
        .inputs
        .iter()
        .map(|input| derive_piece(&store, &db, &narc, &narc_hash, &input.hash))
        .collect();
    let sources: Vec<&[u8]> = piece_bytes.iter().map(Vec::as_slice).collect();
    assert_eq!(materialize(&params, &sources), narc, "bit-faithful rebuild");

    // Members carry their real identities and are CLAIMS (not stored).
    for m in [&a, &b, &c] {
        let h = Blake3::compute(m);
        assert!(
            !store.has(StoreNs::Data, &h),
            "the member is an absent claim, not stored bytes"
        );
        assert!(
            db.get_blob_id(&h).expect("q").is_some(),
            "the member identity is claimed"
        );
    }
    // The empty member grounded the empty literal (the zip rule).
    assert!(store.has(StoreNs::Data, &Blake3::compute(b"")));

    // Settled: a second sweep never re-analyzes the NARC.
    let again = sweep(&mut db, &store);
    assert_eq!(again.positive, 0, "the archive's conclusion is settled");
}

#[test]
fn shared_member_dedups_across_two_narcs() {
    // Two NARCs differing only in one member — the localized-archive
    // shape. Their shared members are one identity, decomposed once.
    let shared1 = pattern(400, 7);
    let shared2 = pattern(600, 8);
    let narc_usa = build_narc(&[&shared1, &shared2, &pattern(256, 100)]);
    let narc_eur = build_narc(&[&shared1, &shared2, &pattern(256, 101)]);

    let (_dir, store, mut db) = world();
    put(&store, &db, &narc_usa);
    put(&store, &db, &narc_eur);
    let report = sweep(&mut db, &store);
    assert_eq!(report.errors, vec![]);
    assert_eq!(report.positive, 2, "both archives decomposed");

    // The shared members are a single claimed identity across both.
    for m in [&shared1, &shared2] {
        let id = db.get_blob_id(&Blake3::compute(m)).expect("q");
        assert!(id.is_some(), "shared member claimed once, dedup across variants");
    }
}

#[test]
fn recipe_volume_cap_keeps_a_huge_narc_literal() {
    let members: Vec<Vec<u8>> = (0..5).map(|i| pattern(16, i as u8)).collect();
    let refs: Vec<&[u8]> = members.iter().map(Vec::as_slice).collect();
    let narc = build_narc(&refs);

    let (_dir, store, mut db) = world();
    // Cap below the member count: the archive stays a literal.
    db.config_set("narc:max-members", b"2").expect("set cap");
    let (_hash, id) = put(&store, &db, &narc);
    let report = sweep(&mut db, &store);
    assert_eq!(report.errors, vec![]);
    assert_eq!((report.positive, report.negative), (0, 1), "over-cap → negative");
    assert!(
        db.recipes_for_output(id).expect("q").is_empty(),
        "no decomposition minted past the cap"
    );
}

#[test]
fn non_narc_blobs_conclude_negative() {
    let (_dir, store, mut db) = world();
    put(&store, &db, b"NARCish but not really, missing the byte-order mark");
    let report = sweep(&mut db, &store);
    assert_eq!(report.errors, vec![]);
    assert_eq!((report.positive, report.negative), (0, 1));
}
