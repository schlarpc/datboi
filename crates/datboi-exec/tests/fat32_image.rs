//! The D62 fsck-in-CI gate, same rank as the golden tests: a wrong FAT
//! chain serves faithfully-wrong bytes that no runtime verification can
//! catch, so every synthesized image is (1) checked by dosfstools'
//! `fsck.vfat -n` and (2) re-read by an independent FAT implementation
//! (the `fatfs` crate, never the writer) and tree-diffed against the
//! view manifest.
//!
//! Gating: when `fsck.vfat` is absent the fsck half skips with a stderr
//! note — unless `DATBOI_REQUIRE_FSCK=1` (set by the nix check
//! derivation), where absence is a failure. CI never skips.

use std::io::{Read as _, Seek as _, Write as _};
use std::process::Command;

use datboi_catalog::{ImageParams, ImageReport, mint_image};
use datboi_core::hash::Blake3;
use datboi_core::viewsnap::{ViewRow, ViewSnapshot};
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Namespace as IndexNs, Residency};
use datboi_store_fs::{Namespace as StoreNs, Store};

struct World {
    dir: tempfile::TempDir,
    store: Store,
    db: Db,
}

fn world() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db = Db::open(dir.path()).expect("db");
    World { dir, store, db }
}

fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (usize::from(seed) * 173 + i * 13) as u8)
        .collect()
}

fn put_content(w: &mut World, bytes: &[u8]) -> Blake3 {
    let hash = Blake3::compute(bytes);
    w.store.put(StoreNs::Data, hash, bytes).expect("put");
    let id =
        w.db.upsert_blob(
            &hash,
            Some(bytes.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .expect("upsert");
    w.db.set_verified(id, 1).expect("verified");
    hash
}

/// The gate's file set: cluster-boundary-straddling sizes, a zero-size
/// file, nested dirs, LFN-forcing names, and an 8.3 collision pair
/// (both mangle to HELLOW~N).
fn gate_files() -> Vec<(String, u64)> {
    vec![
        ("Alpha (USA).gba".to_owned(), 700u64),
        ("empty.sav".to_owned(), 0u64),
        ("exact.bin".to_owned(), 1024u64),
        ("hello world.gba".to_owned(), 300u64),
        ("hello worle.gba".to_owned(), 301u64),
        ("sub/Beta (Europe, Rev 1).gba".to_owned(), 1500u64),
        ("sub/deep/Gamma.gba".to_owned(), 513u64),
    ]
}

fn minted(w: &mut World, name: &str, partition: bool) -> (ImageReport, Vec<ViewRow>) {
    let rows: Vec<ViewRow> = gate_files()
        .iter()
        .enumerate()
        .map(|(i, (path, size))| ViewRow {
            path: path.clone(),
            hash: put_content(
                w,
                &content(
                    u8::try_from(i + 1).expect("small"),
                    usize::try_from(*size).expect("small"),
                ),
            ),
            size: *size,
            seek: 0,
        })
        .collect();
    let snap = ViewSnapshot {
        created_at: 1_780_000_000,
        view_name: name.to_owned(),
        sources: vec![],
        rows: rows.clone(),
    };
    let snap_hash = Blake3::compute(name.as_bytes());
    let report = mint_image(
        &mut w.db,
        &w.store,
        name,
        &snap_hash,
        &snap,
        &ImageParams {
            cluster_size: 512,
            partition,
            label: None,
        },
        true,
        7,
    )
    .expect("mint");
    (report, rows)
}

/// Materialize the image to a file through the real serving path
/// (verified 8 MiB windows), like `view image --out`.
fn export(w: &World, report: &ImageReport, name: &str) -> std::path::PathBuf {
    const WINDOW: u64 = 8 << 20;
    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let path = w.dir.path().join(name);
    let mut out = std::fs::File::create(&path).expect("create");
    let mut off = 0u64;
    while off < report.size {
        let want = WINDOW.min(report.size - off);
        let window = exec
            .serve_range(&w.db, &report.image, off, want)
            .expect("serve");
        assert_eq!(window.len() as u64, want, "short read at {off}");
        out.write_all(&window).expect("write");
        off += want;
    }
    out.sync_all().expect("sync");
    path
}

/// Run `fsck.vfat -n` (read-only). Absent binary: fail under
/// DATBOI_REQUIRE_FSCK=1, skip-with-note otherwise.
fn run_fsck(path: &std::path::Path) {
    match Command::new("fsck.vfat").arg("-n").arg(path).output() {
        Ok(out) => {
            assert!(
                out.status.success(),
                "fsck.vfat found problems in {}:\n{}{}",
                path.display(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            assert!(
                std::env::var_os("DATBOI_REQUIRE_FSCK").is_none_or(|v| v != "1"),
                "DATBOI_REQUIRE_FSCK=1 but fsck.vfat is not installed"
            );
            eprintln!("skipping fsck.vfat (not installed; nix CI enforces it)");
        }
        Err(e) => panic!("running fsck.vfat: {e}"),
    }
}

/// Walk a mounted fatfs tree into (path, size, content-hash) rows.
fn walk<T: fatfs::ReadWriteSeek>(
    dir: &fatfs::Dir<'_, T>,
    prefix: &str,
    out: &mut Vec<(String, u64, Blake3)>,
) {
    for entry in dir.iter() {
        let e = entry.expect("dir entry");
        let name = e.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if e.is_dir() {
            walk(&e.to_dir(), &path, out);
        } else {
            let mut bytes = Vec::new();
            e.to_file().read_to_end(&mut bytes).expect("read file");
            assert_eq!(bytes.len() as u64, e.len(), "{path}: dir entry size");
            out.push((path, e.len(), Blake3::compute(&bytes)));
        }
    }
}

/// Mount with the independent reader and diff the tree against the
/// manifest: exact set equality on (path, size, content hash), plus
/// label and serial.
fn read_back_diff<T: fatfs::ReadWriteSeek>(io: T, rows: &[ViewRow], name: &str, serial: u32) {
    let fs = fatfs::FileSystem::new(io, fatfs::FsOptions::new()).expect("mount");
    assert_eq!(fs.fat_type(), fatfs::FatType::Fat32, "FAT32, not 12/16");
    assert_eq!(fs.volume_id(), serial, "serial from snapshot hash");
    let label = fs.volume_label();
    assert_eq!(label, name.to_uppercase(), "label from view name");

    let mut got = Vec::new();
    walk(&fs.root_dir(), "", &mut got);
    got.sort();
    let mut want: Vec<(String, u64, Blake3)> = rows
        .iter()
        .map(|r| (r.path.clone(), r.size, r.hash))
        .collect();
    want.sort();
    assert_eq!(got, want, "read-back tree == view manifest");
}

#[test]
fn superfloppy_fscks_clean_and_reads_back() {
    let mut w = world();
    let (report, rows) = minted(&mut w, "fscktest", false);
    let path = export(&w, &report, "superfloppy.img");

    run_fsck(&path);

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open image");
    let serial = u32::from_le_bytes(Blake3::compute(b"fscktest").0[0..4].try_into().expect("4"));
    read_back_diff(fscommon::BufStream::new(file), &rows, "fscktest", serial);
}

#[test]
fn partitioned_fscks_clean_and_reads_back() {
    let mut w = world();
    let (report, rows) = minted(&mut w, "fsckpart", true);
    let path = export(&w, &report, "partitioned.img");

    // fsck.vfat takes no offset: check the MBR ourselves, then hand it
    // the partition slice as its own file.
    let mut file = std::fs::File::open(&path).expect("open image");
    let mut mbr = [0u8; 512];
    file.read_exact(&mut mbr).expect("read mbr");
    assert_eq!(&mbr[510..], &[0x55, 0xAA]);
    assert_eq!(mbr[0x1BE + 4], 0x0C, "FAT32-LBA partition type");
    let lba = u64::from(u32::from_le_bytes(
        mbr[0x1BE + 8..0x1BE + 12].try_into().expect("4"),
    ));
    assert_eq!(lba, 2048, "1 MiB alignment");

    let slice_path = w.dir.path().join("partition.img");
    {
        let mut slice = std::fs::File::create(&slice_path).expect("create slice");
        file.seek(std::io::SeekFrom::Start(lba * 512))
            .expect("seek");
        std::io::copy(&mut file, &mut slice).expect("copy slice");
        slice.sync_all().expect("sync");
    }
    run_fsck(&slice_path);

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open image");
    let end = report.size;
    let slice = fscommon::StreamSlice::new(file, lba * 512, end).expect("slice");
    let serial = u32::from_le_bytes(Blake3::compute(b"fsckpart").0[0..4].try_into().expect("4"));
    read_back_diff(fscommon::BufStream::new(slice), &rows, "fsckpart", serial);
}
