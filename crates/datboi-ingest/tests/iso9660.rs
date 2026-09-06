//! iso9660-split (D114) over real stores: synthetic cooked images in
//! the shapes real discs take — nested tree, a uniform pad file, a
//! multi-extent file, a trailing pad, empty files, the piece cap — and
//! the two refusals that keep the family honest (junk, and an image the
//! primary tree does not describe). The critical assertion is the round
//! trip: the minted rebuild recipe, executed through the real assemble
//! reader over piece bytes derived by the minted slice recipes,
//! reproduces the image bit-for-bit.

use std::io::Read as _;

use datboi_core::assemble::{self, AssembleParams, Segment};
use datboi_core::hash::Blake3;
use datboi_core::recipe::{Op, Recipe};
use datboi_index::{Db, Namespace as IndexNs, Residency};
use datboi_ingest::analyzers::Iso9660Analyzer;
use datboi_ingest::iso9660::synth::{self, Spec};
use datboi_ingest::iso9660::{self, SECTOR};
use datboi_ingest::nds::Region;
use datboi_ingest::refine::run_sweep;
use datboi_store_fs::{Namespace as StoreNs, Store};

fn world() -> (tempfile::TempDir, Store, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db = Db::open(dir.path()).expect("db");
    (dir, store, db)
}

fn put(store: &Store, db: &Db, bytes: &[u8]) -> Blake3 {
    let hash = Blake3::compute(bytes);
    store.put(StoreNs::Data, hash, bytes).expect("put");
    db.upsert_blob(
        &hash,
        Some(bytes.len() as u64),
        IndexNs::Data,
        Residency::Resident,
    )
    .expect("row");
    hash
}

fn sweep(db: &mut Db, store: &Store) -> datboi_ingest::refine::SweepReport {
    let exec =
        datboi_exec::Executor::new(store, datboi_exec::ExecConfig::default()).expect("executor");
    let bytes = datboi_ingest::refine::Logical::new(store, &exec);
    let mut analyzer = Iso9660Analyzer;
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
        .query_row(
            "SELECT hash FROM blob WHERE blob_id = ?1",
            (blob_id,),
            |row| row.get(0),
        )
        .expect("blob row");
    Blake3(bytes.try_into().expect("32 bytes"))
}

fn recipes_for(store: &Store, db: &Db, output: &Blake3) -> Vec<Recipe> {
    let blob_id = db
        .get_blob_id(output)
        .expect("query")
        .expect("output blob row");
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

fn derive_piece(store: &Store, db: &Db, img: &[u8], img_hash: &Blake3, piece: &Blake3) -> Vec<u8> {
    let recipe = recipes_for(store, db, piece)
        .into_iter()
        .find(|r| r.inputs.len() == 1 && r.inputs[0].hash == *img_hash)
        .expect("piece has an image-derive recipe");
    let params = AssembleParams::decode(&recipe.params).expect("slice params");
    materialize(&params, &[img])
}

/// Rebuild the image from its minted recipes with no executor: every
/// input is a slice of the image.
fn round_trip(store: &Store, db: &Db, img: &[u8], img_hash: &Blake3) -> (Recipe, Vec<u8>) {
    let rebuild = recipes_for(store, db, img_hash)
        .into_iter()
        .find(|r| r.outputs.len() == 1 && r.outputs[0].hash == *img_hash)
        .expect("rebuild recipe");
    assert!(matches!(&rebuild.op, Op::Builtin { name, major: 1 } if name == "assemble"));
    let params = AssembleParams::decode(&rebuild.params).expect("assemble params");
    let sources: Vec<Vec<u8>> = rebuild
        .inputs
        .iter()
        .map(|input| derive_piece(store, db, img, img_hash, &input.hash))
        .collect();
    let refs: Vec<&[u8]> = sources.iter().map(Vec::as_slice).collect();
    let out = materialize(&params, &refs);
    (rebuild, out)
}

fn pattern(len: usize, salt: u32) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) ^ salt).to_le_bytes()[i % 4])
        .collect()
}

#[test]
fn splits_a_tree_and_round_trips() {
    let system = b"BOOT2 = cdrom0:\\SLUS_000.00;1\nVER = 1.00\nVMODE = NTSC\n".to_vec();
    let big = pattern(300_000, 1);
    let sub = pattern(5_000, 2);
    let dummy = vec![0u8; 3 << 20];
    let movie = pattern(70_000, 3);
    let files: Vec<(&str, &[u8])> = vec![
        ("/SYSTEM.CNF", &system),
        ("/DATA/BIG.BIN", &big),
        ("/DATA/SUB/X.DAT", &sub),
        ("/DUMMY.DAT", &dummy),
        ("/MOVIE/M.PSS", &movie),
    ];
    let image = synth::tree(
        &files,
        &Spec {
            tail_sectors: 100,
            empty: vec!["/EMPTY.TXT"],
            ..Spec::default()
        },
    );
    let (_dir, store, mut db) = world();
    let img_hash = put(&store, &db, &image);

    let report = sweep(&mut db, &store);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.positive, 1, "{:?}", analysis_details(&db));
    let detail = analysis_details(&db).join("\n");
    assert!(detail.contains("5 file(s), 4 dir table(s)"), "{detail}");
    assert!(detail.contains("1 uniform file(s)"), "{detail}");
    assert!(detail.contains("1 empty file(s)"), "{detail}");

    let (rebuild, out) = round_trip(&store, &db, &image, &img_hash);
    assert_eq!(out, image, "rebuild reproduces the image");

    // Every non-uniform file is a named piece: absent claim, slice
    // route, rebuild input.
    for (path, bytes) in files.iter().filter(|(p, _)| *p != "/DUMMY.DAT") {
        let hash = Blake3::compute(bytes);
        let row = db.blob_by_hash(&hash).expect("q").expect("piece claimed");
        assert_eq!(row.residency, Residency::Absent, "{path}");
        assert_eq!(derive_piece(&store, &db, &image, &img_hash, &hash), *bytes);
        assert!(
            rebuild.inputs.iter().any(|i| i.hash == hash),
            "{path} is a rebuild input"
        );
        let slice = recipes_for(&store, &db, &hash)
            .into_iter()
            .find(|r| r.inputs.len() == 1)
            .expect("slice");
        assert_eq!(slice.outputs[0].name.as_deref(), Some(*path));
    }
    // The pad file is a fill, never a claim.
    assert!(
        db.blob_by_hash(&Blake3::compute(&dummy))
            .expect("q")
            .is_none(),
        "a uniform file is not a piece"
    );
    let params = AssembleParams::decode(&rebuild.params).expect("params");
    assert!(
        params
            .segments
            .iter()
            .any(|s| matches!(s, Segment::Fill { byte: 0, len } if *len == dummy.len() as u64)),
        "the pad file is a zero fill of its own length"
    );
    assert!(
        params
            .segments
            .iter()
            .any(|s| matches!(s, Segment::Fill { byte: 0, len } if *len >= 100 * SECTOR)),
        "the trailing pad (with the last file's slack) is a zero fill"
    );
    // The system area is zero here: a fill, not part of a descriptor
    // residue piece.
    assert!(matches!(
        params.segments[0],
        Segment::Fill { byte: 0, len } if len == 16 * SECTOR
    ));
}

#[test]
fn multi_extent_file_is_one_piece_per_extent() {
    let big = pattern(20_000, 7);
    let small = pattern(3_000, 8);
    let files: Vec<(&str, &[u8])> = vec![("/BIG.BIN", &big), ("/SMALL.BIN", &small)];
    let image = synth::tree(
        &files,
        &Spec {
            split: Some(("/BIG.BIN", 3)),
            ..Spec::default()
        },
    );
    let (_dir, store, mut db) = world();
    let img_hash = put(&store, &db, &image);

    let report = sweep(&mut db, &store);
    assert_eq!(report.positive, 1, "{:?}", analysis_details(&db));
    let detail = analysis_details(&db).join("\n");
    assert!(detail.contains("2 file(s), 1 dir table(s)"), "{detail}");
    assert!(detail.contains("1 multi-extent file(s)"), "{detail}");

    let (rebuild, out) = round_trip(&store, &db, &image, &img_hash);
    assert_eq!(out, image);
    // Extents are 3 sectors, 3 sectors, remainder — each its own piece,
    // named by ordinal.
    let extents = [
        &big[..3 * SECTOR as usize],
        &big[3 * SECTOR as usize..6 * SECTOR as usize],
        &big[6 * SECTOR as usize..],
    ];
    for (k, bytes) in extents.iter().enumerate() {
        let hash = Blake3::compute(bytes);
        assert!(rebuild.inputs.iter().any(|i| i.hash == hash), "extent {k}");
        let slice = recipes_for(&store, &db, &hash)
            .into_iter()
            .find(|r| r.inputs.len() == 1)
            .expect("slice");
        let want = if k == 0 {
            "/BIG.BIN".to_owned()
        } else {
            format!("/BIG.BIN#{k}")
        };
        assert_eq!(slice.outputs[0].name.as_deref(), Some(want.as_str()));
    }
    assert!(
        !rebuild
            .inputs
            .iter()
            .any(|i| i.hash == Blake3::compute(&big))
    );
}

#[test]
fn junk_and_undescribed_images_are_negative() {
    let (_dir, store, mut db) = world();
    let junk = pattern(200 * SECTOR as usize, 99);
    put(&store, &db, &junk);
    // A real tree naming a sliver of an image that is mostly bytes the
    // tree does not describe (the redump Xbox shape: a DVD-Video volume
    // in front of a game partition).
    let small = pattern(4_000, 5);
    let files: Vec<(&str, &[u8])> = vec![("/VIDEO_TS/VIDEO_TS.IFO", &small)];
    let stub = synth::tree(
        &files,
        &Spec {
            junk_sectors: 2200,
            ..Spec::default()
        },
    );
    put(&store, &db, &stub);

    let report = sweep(&mut db, &store);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.positive, 0);
    assert_eq!(report.negative, 2);
    let details = analysis_details(&db);
    assert!(
        details
            .iter()
            .any(|d| d.contains("no iso9660 primary volume descriptor")),
        "{details:?}"
    );
    assert!(
        details.iter().any(|d| d.contains("not iso9660-shaped")),
        "{details:?}"
    );
    // Nothing was claimed for either.
    assert!(
        db.blob_by_hash(&Blake3::compute(&small))
            .expect("q")
            .is_none()
    );
}

#[test]
fn piece_cap_coalesces_to_extents() {
    let bytes: Vec<Vec<u8>> = (0..6)
        .map(|i| pattern(10_000 + i * 100, 40 + i as u32))
        .collect();
    let names = ["/A.BIN", "/B.BIN", "/C.BIN", "/D.BIN", "/E.BIN", "/F.BIN"];
    let files: Vec<(&str, &[u8])> = names
        .iter()
        .zip(&bytes)
        .map(|(n, b)| (*n, b.as_slice()))
        .collect();
    let image = synth::tree(&files, &Spec::default());
    let (_dir, store, mut db) = world();
    db.config_set("iso9660:max-pieces", b"4").expect("policy");
    let img_hash = put(&store, &db, &image);

    let report = sweep(&mut db, &store);
    assert_eq!(report.positive, 1, "{:?}", analysis_details(&db));
    let detail = analysis_details(&db).join("\n");
    assert!(detail.contains("coalesced into extents"), "{detail}");

    let (rebuild, out) = round_trip(&store, &db, &image, &img_hash);
    assert_eq!(out, image);
    // Files are sector-contiguous, so the run is ONE extent piece from
    // the root table through the last file's sector; no per-file claims.
    assert_eq!(rebuild.inputs.len(), 1, "{:?}", rebuild.inputs);
    assert!(
        db.blob_by_hash(&Blake3::compute(&bytes[0]))
            .expect("q")
            .is_none()
    );
    let slice = recipes_for(&store, &db, &rebuild.inputs[0].hash)
        .into_iter()
        .find(|r| r.inputs.len() == 1)
        .expect("slice");
    assert!(
        slice.outputs[0]
            .name
            .as_deref()
            .is_some_and(|n| n.starts_with("extent@")),
        "{:?}",
        slice.outputs[0].name
    );
}

/// Real-image check, opt-in: `DATBOI_ISO9660_IMAGE=/path/to.iso cargo
/// test -p datboi-ingest --test iso9660 -- --ignored --nocapture`.
/// Prints the layout summary; asserts only structural sanity.
#[test]
#[ignore]
fn real_image_from_env() {
    let Ok(path) = std::env::var("DATBOI_ISO9660_IMAGE") else {
        eprintln!("DATBOI_ISO9660_IMAGE unset");
        return;
    };
    let mut file = std::fs::File::open(&path).expect("open image");
    let t0 = std::time::Instant::now();
    let layout = match iso9660::parse_layout(&mut file, 4096) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{path}: {e} ({:.1?})", t0.elapsed());
            return;
        }
    };
    let total: u64 = layout
        .regions
        .iter()
        .map(|r| match r {
            Region::Piece(ix) => layout.pieces[*ix].len,
            Region::Fill { len, .. } | Region::Literal { len, .. } | Region::Extern { len, .. } => {
                *len
            }
        })
        .sum();
    assert_eq!(total, layout.total_len);
    eprintln!(
        "{path}: {} pieces ({} files, {} dirs, {} uniform, {} multi-extent, {} empty), declared {} of {} sectors, fill {} B, residue {} B, coalesced {}, {} regions, {:.1?}",
        layout.pieces.len(),
        layout.file_count,
        layout.dir_count,
        layout.uniform_files,
        layout.multi_extent_files,
        layout.empty_files,
        layout.declared_sectors,
        layout.total_len / SECTOR,
        layout.fill_bytes,
        layout.residual_bytes,
        layout.coalesced,
        layout.regions.len(),
        t0.elapsed()
    );
    for p in layout.pieces.iter().filter(|p| p.name.starts_with("gap@")) {
        eprintln!("  residue {} ({} B)", p.name, p.len);
    }
}
