//! gcm-split (D115) over real stores: the synthetic GameCube fixture
//! with real junk, with zero padding, and the refusals. The critical
//! assertion is the round trip — the minted rebuild recipe, executed
//! through the real assemble reader over piece bytes derived by the
//! minted slice recipes plus a NATIVELY regenerated junk stream,
//! reproduces the image bit-for-bit — and that the junk claim is a
//! zero-input recipe pinning the component.

use std::io::Read as _;

use datboi_core::assemble::{self, AssembleParams, Segment};
use datboi_core::hash::Blake3;
use datboi_core::recipe::{Op, Recipe};
use datboi_index::{Db, Namespace as IndexNs, Residency};
use datboi_ingest::analyzers::GcmAnalyzer;
use datboi_ingest::gcm::synth::{self, Pad};
use datboi_ingest::gcm::{self, SECTOR};
use datboi_ingest::nds::Region;
use datboi_ingest::refine::run_sweep;
use datboi_store_fs::{Namespace as StoreNs, Store};
use datboi_xf_gc_junk::{Lfg, fill_at};

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
    let mut analyzer = GcmAnalyzer::new();
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

/// Rebuild the image from its minted recipes with no executor: pieces
/// by slice, the junk by the native generator (the component's twin).
fn round_trip(store: &Store, db: &Db, img: &[u8], img_hash: &Blake3) -> (Recipe, Vec<u8>) {
    let rebuild = recipes_for(store, db, img_hash)
        .into_iter()
        .find(|r| r.outputs.len() == 1 && r.outputs[0].hash == *img_hash)
        .expect("rebuild recipe");
    assert!(matches!(&rebuild.op, Op::Builtin { name, major: 1 } if name == "assemble"));
    let params = AssembleParams::decode(&rebuild.params).expect("assemble params");
    let mut sources: Vec<Vec<u8>> = Vec::new();
    for input in &rebuild.inputs {
        if input.role.as_deref() == Some("gc-junk") {
            let fill = recipes_for(store, db, &input.hash)
                .into_iter()
                .find(|r| r.inputs.is_empty())
                .expect("junk has a zero-input recipe");
            let Op::Wasm {
                component, export, ..
            } = &fill.op
            else {
                panic!("junk route is wasm")
            };
            assert_eq!(*component, GcmAnalyzer::component_hash());
            assert_eq!(export, "fill");
            let p = datboi_xf_gc_junk::params::Params::decode(&fill.params).expect("params");
            let mut stream = vec![0u8; usize::try_from(p.len).expect("small")];
            fill_at(&mut Lfg::default(), p.id, p.disc, 0, &mut stream);
            assert_eq!(Blake3::compute(&stream), input.hash, "junk identity");
            assert_eq!(stream.len() as u64, fill.outputs[0].size);
            sources.push(stream);
        } else {
            sources.push(derive_piece(store, db, img, img_hash, &input.hash));
        }
    }
    let refs: Vec<&[u8]> = sources.iter().map(Vec::as_slice).collect();
    let out = materialize(&params, &refs);
    (rebuild, out)
}

fn pattern(len: usize, salt: u32) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) ^ salt).to_le_bytes()[i % 4])
        .collect()
}

fn files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("/opening.bnr", pattern(6496, 1)),
        ("/data/big.arc", pattern(300_000, 2)),
        ("/data/small.dat", pattern(700, 3)),
        ("/audio/stream.adp", pattern(90_000, 4)),
        ("/empty.txt", Vec::new()),
        ("/last.bin", pattern(40_001, 5)),
    ]
}

#[test]
fn junk_image_splits_regenerates_junk_and_round_trips() {
    let image = synth::image(*b"GALE01", 0, &files(), Pad::Junk);
    let (_dir, store, mut db) = world();
    let img_hash = put(&store, &db, &image.bytes);

    let report = sweep(&mut db, &store);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.positive, 1, "{:?}", analysis_details(&db));
    let detail = analysis_details(&db).join("\n");
    assert!(detail.contains("5 file(s), 3 dir(s), 1 empty"), "{detail}");
    assert!(detail.contains("game GALE01 disc 0"), "{detail}");
    assert!(detail.contains("regenerated"), "{detail}");

    let (rebuild, out) = round_trip(&store, &db, &image.bytes, &img_hash);
    assert_eq!(out, image.bytes, "rebuild reproduces the image");

    // Every non-empty file and system piece is a named piece with an
    // absent claim and a slice route.
    let mut expect: Vec<(String, Vec<u8>)> = image
        .files
        .iter()
        .filter(|(_, b)| !b.is_empty())
        .cloned()
        .collect();
    expect.push(("sys/apploader.img".into(), image.apploader.clone()));
    expect.push(("sys/main.dol".into(), image.dol.clone()));
    for (path, bytes) in &expect {
        let hash = Blake3::compute(bytes);
        let row = db.blob_by_hash(&hash).expect("q").expect("piece claimed");
        assert_eq!(row.residency, Residency::Absent, "{path}");
        assert_eq!(
            derive_piece(&store, &db, &image.bytes, &img_hash, &hash),
            *bytes
        );
        assert!(
            rebuild.inputs.iter().any(|i| i.hash == hash),
            "{path} is a rebuild input"
        );
        let slice = recipes_for(&store, &db, &hash)
            .into_iter()
            .find(|r| r.inputs.len() == 1)
            .expect("slice");
        assert_eq!(slice.outputs[0].name.as_deref(), Some(path.as_str()));
    }
    // The junk input spans the whole address space; the residue gap is
    // a piece; the zero pad is a fill; junk segments sit at their own
    // offsets (an Extern range's offset equals its disc position).
    let junk = rebuild
        .inputs
        .iter()
        .find(|i| i.role.as_deref() == Some("gc-junk"))
        .expect("junk input");
    let fill = recipes_for(&store, &db, &junk.hash)
        .into_iter()
        .find(|r| r.inputs.is_empty())
        .expect("zero-input junk recipe");
    assert_eq!(fill.outputs[0].size, image.bytes.len() as u64);
    let (rs, rl) = image.residue;
    let residue = Blake3::compute(&image.bytes[rs as usize..(rs + rl) as usize]);
    assert!(
        rebuild.inputs.iter().any(|i| i.hash == residue),
        "residue gap is a piece"
    );
    let params = AssembleParams::decode(&rebuild.params).expect("params");
    let junk_ix = rebuild
        .inputs
        .iter()
        .position(|i| i.hash == junk.hash)
        .expect("ix") as u32;
    let mut cursor = 0u64;
    let mut junk_segments = 0;
    for s in &params.segments {
        match s {
            Segment::BlobRange {
                input_ix,
                offset,
                len,
            } => {
                if *input_ix == junk_ix {
                    assert_eq!(*offset, cursor, "junk at its own offset");
                    junk_segments += 1;
                }
                cursor += len;
            }
            Segment::Fill { len, .. } => cursor += len,
            Segment::Literal { bytes } => cursor += bytes.len() as u64,
        }
    }
    assert_eq!(cursor, image.bytes.len() as u64);
    assert!(
        junk_segments >= 4,
        "junk between files and after the pad: {junk_segments}"
    );
    assert!(
        params
            .segments
            .iter()
            .any(|s| matches!(s, Segment::Fill { byte: 0, len } if *len == 3 * SECTOR + 17)),
        "zero pad is a fill"
    );
}

/// A zero-padded (or scrubbed) master claims no junk at all: fills
/// only, no component published, no zero-input recipe.
#[test]
fn zero_padded_image_claims_no_junk() {
    let image = synth::image(*b"GALE01", 0, &files(), Pad::Zero);
    let (_dir, store, mut db) = world();
    let img_hash = put(&store, &db, &image.bytes);

    let report = sweep(&mut db, &store);
    assert_eq!(report.positive, 1, "{:?}", analysis_details(&db));
    let detail = analysis_details(&db).join("\n");
    assert!(
        detail.contains("junk 0 B (0.0%) — none matched"),
        "{detail}"
    );
    let (rebuild, out) = round_trip(&store, &db, &image.bytes, &img_hash);
    assert_eq!(out, image.bytes);
    assert!(
        rebuild
            .inputs
            .iter()
            .all(|i| i.role.as_deref() != Some("gc-junk"))
    );
    assert!(!store.has(StoreNs::Data, &GcmAnalyzer::component_hash()));
}

#[test]
fn non_gamecube_bytes_are_negative() {
    let (_dir, store, mut db) = world();
    let junk = pattern(200_000, 99);
    put(&store, &db, &junk);
    let mut wii = synth::image(*b"RSBE01", 0, &files(), Pad::Zero).bytes;
    wii[0x18..0x1C].copy_from_slice(&gcm::WII_MAGIC);
    wii[0x1C..0x20].fill(0);
    put(&store, &db, &wii);

    let report = sweep(&mut db, &store);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.negative, 2);
    let details = analysis_details(&db);
    assert!(
        details.iter().any(|d| d.contains("not a gamecube disc")),
        "{details:?}"
    );
    assert!(
        details.iter().any(|d| d.contains("a wii disc")),
        "{details:?}"
    );
}

#[test]
fn piece_cap_coalesces_to_extents() {
    let image = synth::image(*b"GALE01", 0, &files(), Pad::Junk);
    let (_dir, store, mut db) = world();
    db.config_set("gcm:max-pieces", b"3").expect("policy");
    let img_hash = put(&store, &db, &image.bytes);

    let report = sweep(&mut db, &store);
    assert_eq!(report.positive, 1, "{:?}", analysis_details(&db));
    let detail = analysis_details(&db).join("\n");
    assert!(detail.contains("coalesced into extents"), "{detail}");
    let (_rebuild, out) = round_trip(&store, &db, &image.bytes, &img_hash);
    assert_eq!(out, image.bytes);
    assert!(
        db.blob_by_hash(&Blake3::compute(&image.files[1].1))
            .expect("q")
            .is_none(),
        "no per-file claim past the cap"
    );
}

/// Real-image check, opt-in: `DATBOI_GCM_IMAGE=/path/to.iso cargo test
/// -p datboi-ingest --test gcm -- --ignored --nocapture`. Prints the
/// layout summary; asserts only structural sanity.
#[test]
#[ignore]
fn real_image_from_env() {
    let Ok(path) = std::env::var("DATBOI_GCM_IMAGE") else {
        eprintln!("DATBOI_GCM_IMAGE unset");
        return;
    };
    let mut file = std::fs::File::open(&path).expect("open image");
    let t0 = std::time::Instant::now();
    let layout = match gcm::parse_layout(&mut file, 4096) {
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
        "{path}: game {} disc {}, {} pieces ({} files, {} dirs, {} empty), junk {} B ({:.1}%), fill {} B, residue {} B, coalesced {}, {} regions, {:.1?}",
        String::from_utf8_lossy(&layout.game_id),
        layout.disc,
        layout.pieces.len(),
        layout.file_count,
        layout.dir_count,
        layout.empty_files,
        layout.junk_bytes,
        layout.junk_bytes as f64 * 100.0 / layout.total_len as f64,
        layout.fill_bytes,
        layout.residual_bytes,
        layout.coalesced,
        layout.regions.len(),
        t0.elapsed()
    );
    let residue: Vec<_> = layout
        .pieces
        .iter()
        .filter(|p| p.name.starts_with("gap@"))
        .collect();
    for p in residue.iter().take(12) {
        eprintln!("  residue {} ({} B)", p.name, p.len);
    }
    if residue.len() > 12 {
        eprintln!("  … {} residue pieces in all", residue.len());
    }
}
