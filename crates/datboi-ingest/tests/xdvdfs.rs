//! xdvdfs-split (D111) over real stores: the synthetic XISO fixture in
//! its seed-era, rc4-era, and trimmed shapes. The critical assertion is
//! the round trip — the minted rebuild recipe, executed through the
//! real assemble reader over piece bytes derived by the minted slice
//! recipes plus a NATIVELY regenerated filler stream, reproduces the
//! image bit-for-bit — and that the filler claim is a zero-input
//! recipe pinning the component.

use std::io::{Cursor, Read as _};

use datboi_core::assemble::{self, AssembleParams, Segment};
use datboi_core::hash::Blake3;
use datboi_core::recipe::{Op, Recipe};
use datboi_index::{Db, Namespace as IndexNs, Residency};
use datboi_ingest::analyzers::XdvdfsAnalyzer;
use datboi_ingest::nds::Region;
use datboi_ingest::refine::run_sweep;
use datboi_ingest::xdvdfs::synth::{self, Filler};
use datboi_ingest::xdvdfs::{self, SECTOR, parse_layout};
use datboi_store_fs::{Namespace as StoreNs, Store};
use datboi_xf_xgd1_prng::{Prng, SECTOR_LEN};

const SEED: u32 = 0x4E99_8EB0;

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
    let mut analyzer = XdvdfsAnalyzer::new();
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

fn native_stream(seed: u32, sectors: u64) -> Vec<u8> {
    let mut prng = Prng::new(seed, 0);
    let mut out = Vec::with_capacity(usize::try_from(sectors).expect("small") * SECTOR_LEN);
    let mut s = [0u8; SECTOR_LEN];
    for _ in 0..sectors {
        prng.fill_sector(&mut s);
        out.extend_from_slice(&s);
    }
    out
}

/// Rebuild the image from its minted recipes with no executor: pieces
/// by slice, the filler by the native generator (the component's twin).
fn round_trip(store: &Store, db: &Db, img: &[u8], img_hash: &Blake3) -> (Recipe, Vec<u8>) {
    let rebuild = recipes_for(store, db, img_hash)
        .into_iter()
        .find(|r| r.outputs.len() == 1 && r.outputs[0].hash == *img_hash)
        .expect("rebuild recipe");
    assert!(matches!(&rebuild.op, Op::Builtin { name, major: 1 } if name == "assemble"));
    let params = AssembleParams::decode(&rebuild.params).expect("assemble params");
    let mut sources: Vec<Vec<u8>> = Vec::new();
    for input in &rebuild.inputs {
        if input.role.as_deref() == Some("xgd1-filler") {
            let fill = recipes_for(store, db, &input.hash)
                .into_iter()
                .find(|r| r.inputs.is_empty())
                .expect("filler has a zero-input recipe");
            let Op::Wasm {
                component, export, ..
            } = &fill.op
            else {
                panic!("filler route is wasm")
            };
            assert_eq!(*component, XdvdfsAnalyzer::component_hash());
            assert_eq!(export, "fill");
            let p = datboi_xf_xgd1_prng::params::Params::decode(&fill.params).expect("params");
            let stream = native_stream(p.seed, p.sectors);
            assert_eq!(Blake3::compute(&stream), input.hash, "filler identity");
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

#[test]
fn seed_era_image_splits_regenerates_filler_and_round_trips() {
    let image = synth::image(Filler::Seed(SEED), false);
    let (_dir, store, mut db) = world();
    let img_hash = put(&store, &db, &image.bytes);

    let report = sweep(&mut db, &store);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.positive, 1, "{:?}", analysis_details(&db));
    let detail = analysis_details(&db).join("\n");
    assert!(detail.contains("filler seed 0x4e998eb0"), "{detail}");
    assert!(
        detail.contains("4096 security-range sector(s) consumed"),
        "{detail}"
    );
    assert!(detail.contains("3 file(s), 2 dir table(s)"), "{detail}");
    assert!(detail.contains("layout tool build 3926"), "{detail}");

    let (rebuild, out) = round_trip(&store, &db, &image.bytes, &img_hash);
    assert_eq!(out, image.bytes, "rebuild reproduces the image");

    // Every file is a named piece with an absent claim and a slice route.
    for (path, bytes) in &image.files {
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
    }
    // The filler input claims exactly the consumed stream, and the
    // residue tail is a gap piece (over the literal cap), never junk in
    // the stream.
    let filler = rebuild
        .inputs
        .iter()
        .find(|i| i.role.as_deref() == Some("xgd1-filler"))
        .expect("filler input");
    let fill = recipes_for(&store, &db, &filler.hash)
        .into_iter()
        .find(|r| r.inputs.is_empty())
        .expect("zero-input filler recipe");
    assert_eq!(fill.outputs[0].size, image.stream_sectors * SECTOR);
    let tail =
        Blake3::compute(&image.bytes[usize::try_from(image.residue_sector * SECTOR).unwrap()..]);
    assert!(
        rebuild.inputs.iter().any(|i| i.hash == tail),
        "residue tail is a piece"
    );

    // Segment shape: the security range is a zero fill and the stream
    // ranges around it are contiguous in stream space (35..39 then
    // 4135..4139), so the regenerated bytes never land in storage.
    let params = AssembleParams::decode(&rebuild.params).expect("params");
    let filler_ix = rebuild
        .inputs
        .iter()
        .position(|i| i.hash == filler.hash)
        .expect("ix") as u32;
    let stream_ranges: Vec<(u64, u64)> = params
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::BlobRange {
                input_ix,
                offset,
                len,
            } if *input_ix == filler_ix => Some((*offset / SECTOR, *len / SECTOR)),
            _ => None,
        })
        .collect();
    assert_eq!(
        stream_ranges,
        vec![(0, 32), (32, 1), (33, 2), (35, 4), (4135, 4)]
    );
    assert!(
        params
            .segments
            .iter()
            .any(|s| matches!(s, Segment::Fill { byte: 0, len } if *len == 4096 * SECTOR)),
        "security range is a zero fill"
    );
}

#[test]
fn rc4_era_image_splits_with_literal_filler() {
    let image = synth::image(Filler::Random, false);
    let (_dir, store, mut db) = world();
    let img_hash = put(&store, &db, &image.bytes);

    let report = sweep(&mut db, &store);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.positive, 1, "{:?}", analysis_details(&db));
    let detail = analysis_details(&db).join("\n");
    assert!(detail.contains("no seed-era filler"), "{detail}");

    let (rebuild, out) = round_trip(&store, &db, &image.bytes, &img_hash);
    assert_eq!(out, image.bytes);
    assert!(
        rebuild.inputs.iter().all(|i| i.role.is_none()),
        "no filler input without a seed"
    );
    // The gaps are pieces (or literals), never claimed as stream.
    assert!(
        !recipes_for(&store, &db, &img_hash)
            .iter()
            .any(|r| r.inputs.is_empty())
    );
}

#[test]
fn trimmed_xiso_splits_and_round_trips() {
    let image = synth::image(Filler::Seed(SEED), true);
    let (_dir, store, mut db) = world();
    let img_hash = put(&store, &db, &image.bytes);
    let report = sweep(&mut db, &store);
    assert_eq!(report.positive, 1, "{:?}", analysis_details(&db));
    let (_, out) = round_trip(&store, &db, &image.bytes, &img_hash);
    assert_eq!(out, image.bytes);
}

#[test]
fn layout_classifies_every_shape() {
    let image = synth::image(Filler::Seed(SEED), false);
    let layout = parse_layout(&mut Cursor::new(&image.bytes), 4096).expect("layout");
    assert_eq!(layout.base, 0);
    assert_eq!(layout.file_count, 3);
    assert_eq!(layout.dir_count, 2);
    assert_eq!(layout.empty_files, 1);
    assert_eq!(layout.layout_tool_build, Some(3926));
    assert!(!layout.coalesced);
    let filler = layout.filler.expect("seed recovered");
    assert_eq!(filler.seed, SEED);
    assert_eq!(filler.sectors, image.stream_sectors);
    assert_eq!(layout.stream_sectors, 43);
    assert_eq!(layout.security_sectors, 4096);
    // Slack tails: a.bin (1096 B), c.bin (2047 B); pad: 3 sectors; the
    // security range: 4096 sectors.
    assert_eq!(layout.fill_bytes, 1096 + 2047 + 3 * SECTOR + 4096 * SECTOR);
    assert_eq!(layout.residual_bytes, 3 * SECTOR);
    // Regions concatenate to the whole image.
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
    assert_eq!(total, image.bytes.len() as u64);
    let names: Vec<&str> = layout.pieces.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "dir:/",
            "/a.bin",
            "dir:/sub",
            "/sub/b.bin",
            "/sub/c.bin",
            "gap@0x00081c000"
        ]
    );
}

#[test]
fn piece_cap_coalesces_into_extents() {
    let image = synth::image(Filler::Seed(SEED), false);
    let layout = parse_layout(&mut Cursor::new(&image.bytes), 2).expect("layout");
    assert!(layout.coalesced);
    // 34..38 (root table, filler at 35 is NOT data — so two runs:
    // 34, then 36..38), 40..45 (sub table + b + c), plus the residue.
    let names: Vec<&str> = layout.pieces.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "extent@0x000011000",
            "extent@0x000012000",
            "extent@0x000014000",
            "gap@0x00081c000"
        ]
    );
    assert_eq!(layout.filler.expect("seed").sectors, image.stream_sectors);
}

#[test]
fn refuses_non_images_and_broken_trees() {
    let junk = vec![0x55u8; 200 * 1024];
    let err = parse_layout(&mut Cursor::new(&junk), 4096).expect_err("not xdvdfs");
    assert!(matches!(
        err,
        xdvdfs::XdvdfsError::Refused(xdvdfs::Refusal::NotXdvdfs)
    ));

    // A root table whose entry points past the image.
    let mut image = synth::image(Filler::Seed(SEED), true).bytes;
    let root = 34 * SECTOR_LEN;
    image[root + 4..root + 8].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
    let err = parse_layout(&mut Cursor::new(&image), 4096).expect_err("bounds");
    assert!(matches!(err, xdvdfs::XdvdfsError::Refused(_)), "{err}");

    // A self-linking entry (right link back to itself).
    let mut image = synth::image(Filler::Seed(SEED), true).bytes;
    image[root + 2..root + 4].copy_from_slice(&0u16.to_le_bytes());
    image[root..root + 2].copy_from_slice(&0u16.to_le_bytes());
    // Make the FIRST entry's right link point at offset 0 (itself).
    image[root + 2..root + 4].copy_from_slice(&0u16.to_le_bytes());
    // (a zero link is "none"; forge a cycle via the second entry instead)
    let second = root + 20; // "a.bin" entry is 14 + 5 -> padded to 20
    image[second + 2..second + 4].copy_from_slice(&5u16.to_le_bytes()); // -> offset 20 = itself
    image[root + 2..root + 4].copy_from_slice(&5u16.to_le_bytes());
    let err = parse_layout(&mut Cursor::new(&image), 4096).expect_err("cycle");
    assert!(
        matches!(err, xdvdfs::XdvdfsError::Refused(xdvdfs::Refusal::Directory(ref m)) if m.contains("linked twice")),
        "{err}"
    );
}

/// Real-image check, opt-in: `DATBOI_XDVDFS_IMAGE=/path/to.iso cargo
/// test -p datboi-ingest --test xdvdfs -- --ignored --nocapture`.
/// Prints the layout summary; asserts only structural sanity.
#[test]
#[ignore]
fn real_image_from_env() {
    let Ok(path) = std::env::var("DATBOI_XDVDFS_IMAGE") else {
        eprintln!("DATBOI_XDVDFS_IMAGE unset");
        return;
    };
    let mut file = std::fs::File::open(&path).expect("open image");
    let t0 = std::time::Instant::now();
    let layout = parse_layout(&mut file, 4096).expect("layout");
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
        "{path}: base {:#x}, {} pieces ({} files, {} dirs), filler {:?}, stream {} + security {} sectors, fill {} B, residue {} B, build {:?}, {} regions, {:.1?}",
        layout.base,
        layout.pieces.len(),
        layout.file_count,
        layout.dir_count,
        layout.filler.map(|f| (format!("{:#x}", f.seed), f.sectors)),
        layout.stream_sectors,
        layout.security_sectors,
        layout.fill_bytes,
        layout.residual_bytes,
        layout.layout_tool_build,
        layout.regions.len(),
        t0.elapsed()
    );
    for p in layout.pieces.iter().filter(|p| p.name.starts_with("gap@")) {
        eprintln!("  residue {} ({} B)", p.name, p.len);
    }
}
