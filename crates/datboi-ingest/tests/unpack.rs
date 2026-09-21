//! D123: unpacking a transport container — at the door (`--unpack`) and
//! after the fact (`datboi unpack`). Both doors must converge on ONE
//! graph: members resident and byte-correct, the archive's bytes gone,
//! its row `Absent` with aliases and `source_file` provenance intact,
//! and its `container->member` recipes still there as the edge from rom
//! to archive — inert to the planner and to `is_evictable`, which is
//! what this file pins rather than leaves to be re-derived.

use std::fs;
use std::io::{Read, Write};

use datboi_core::hash::Blake3;
use datboi_index::{Db, Residency};
use datboi_ingest::unpack::{UnpackOptions, UnpackReport, unpack_corpus};
use datboi_ingest::{IngestConfig, Ingester};
use datboi_store_fs::{Namespace as StoreNs, Store};
use flate2::Compression;
use flate2::write::DeflateEncoder;

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

fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// Hand-written zip builder — deterministic bytes we control exactly.
struct ZipBuilder {
    data: Vec<u8>,
    central: Vec<u8>,
    entries: u16,
}

impl ZipBuilder {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            central: Vec::new(),
            entries: 0,
        }
    }

    fn add(&mut self, name: &str, contents: &[u8], deflate: bool) -> &mut Self {
        self.add_raw(name, contents, if deflate { 8 } else { 0 }, 0)
    }

    fn add_raw(&mut self, name: &str, contents: &[u8], method: u16, flags: u16) -> &mut Self {
        let payload = if method == 8 {
            let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
            enc.write_all(contents).expect("deflate");
            enc.finish().expect("deflate finish")
        } else {
            contents.to_vec()
        };
        let crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(contents);
            h.finalize()
        };
        let local_offset = self.data.len() as u32;
        self.data.extend_from_slice(b"PK\x03\x04");
        self.data.extend_from_slice(&20u16.to_le_bytes());
        self.data.extend_from_slice(&flags.to_le_bytes());
        self.data.extend_from_slice(&method.to_le_bytes());
        self.data.extend_from_slice(&[0; 4]);
        self.data.extend_from_slice(&crc.to_le_bytes());
        self.data
            .extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.data
            .extend_from_slice(&(contents.len() as u32).to_le_bytes());
        self.data
            .extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.data.extend_from_slice(&0u16.to_le_bytes());
        self.data.extend_from_slice(name.as_bytes());
        self.data.extend_from_slice(&payload);

        self.central.extend_from_slice(b"PK\x01\x02");
        self.central.extend_from_slice(&20u16.to_le_bytes());
        self.central.extend_from_slice(&20u16.to_le_bytes());
        self.central.extend_from_slice(&flags.to_le_bytes());
        self.central.extend_from_slice(&method.to_le_bytes());
        self.central.extend_from_slice(&[0; 4]);
        self.central.extend_from_slice(&crc.to_le_bytes());
        self.central
            .extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.central
            .extend_from_slice(&(contents.len() as u32).to_le_bytes());
        self.central
            .extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.central.extend_from_slice(&[0; 2]);
        self.central.extend_from_slice(&[0; 2]);
        self.central.extend_from_slice(&[0; 2]);
        self.central.extend_from_slice(&[0; 2]);
        self.central.extend_from_slice(&[0; 4]);
        self.central.extend_from_slice(&local_offset.to_le_bytes());
        self.central.extend_from_slice(name.as_bytes());
        self.entries += 1;
        self
    }

    fn finish(&self) -> Vec<u8> {
        let mut out = self.data.clone();
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&self.central);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&self.entries.to_le_bytes());
        out.extend_from_slice(&self.entries.to_le_bytes());
        out.extend_from_slice(&(self.central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&[0; 2]);
        out
    }
}

/// Two members, one STORED (affine route) and one DEFLATE (opaque
/// route) — both classes of zip member in one container.
fn two_member_zip() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let stored = pattern(40_000, 0x1234_5678_9abc_def0);
    let deflated = pattern(50_000, 0x0fed_cba9_8765_4321);
    let mut zb = ZipBuilder::new();
    zb.add("a.rom", &stored, false);
    zb.add("dir/b.rom", &deflated, true);
    (zb.finish(), stored, deflated)
}

fn bytes_of(store: &Store, hash: &Blake3) -> Vec<u8> {
    let mut out = Vec::new();
    store
        .get(StoreNs::Data, hash)
        .expect("get")
        .expect("resident")
        .read_to_end(&mut out)
        .expect("read");
    out
}

/// Every assertion D123 makes about a container after its members are
/// out and its bytes are gone.
fn assert_unpacked(w: &World, container: &Blake3, members: &[&[u8]], source_path: &str) {
    assert!(
        !w.store.has(StoreNs::Data, container),
        "the archive's bytes are gone"
    );
    let row =
        w.db.blob_by_hash(container)
            .expect("q")
            .expect("the container keeps its row");
    assert_eq!(
        row.residency,
        Residency::Absent,
        "Absent, not EvictedCovered: nothing covers these bytes"
    );

    // Provenance: the alias tuple and the source path both survive.
    let aliases: i64 =
        w.db.cache()
            .query_row(
                "SELECT COUNT(*) FROM alias WHERE blob_id = ?1",
                [row.blob_id],
                |r| r.get(0),
            )
            .expect("alias count");
    assert!(aliases > 0, "the container keeps its alias tuple");
    let path: String =
        w.db.cache()
            .query_row(
                "SELECT path FROM source_file WHERE blob_id = ?1",
                [row.blob_id],
                |r| r.get(0),
            )
            .expect("source_file row survives the drop");
    assert!(
        path.ends_with(source_path),
        "provenance still says where the bytes arrived: {path}"
    );

    // The members are resident, byte-correct, and the recipes that once
    // derived them are still on record.
    let claims =
        w.db.container_member_claims(row.blob_id)
            .expect("member claims");
    assert_eq!(
        claims.len(),
        members.len(),
        "every member is still claimed from this container"
    );
    for want in members {
        let hash = Blake3::compute(want);
        assert_eq!(&bytes_of(&w.store, &hash), want, "member bytes bit-exact");
        let member = w.db.blob_by_hash(&hash).expect("q").expect("indexed");
        assert_eq!(member.residency, Residency::Resident);
        assert!(
            !w.db
                .recipes_for_output(member.blob_id)
                .expect("q")
                .is_empty(),
            "the container->member recipe stays as the provenance edge"
        );
        // D123's mechanical claim: the dangling route cannot ground the
        // member, so the only copy of these bytes is never evictable.
        assert!(
            !w.db.is_evictable(member.blob_id).expect("q"),
            "an unpacked member is the only copy and must not be evictable"
        );
    }

    // And the planner will not serve THROUGH the dead route: the
    // container itself has no way back.
    let exec =
        datboi_exec::Executor::new(&w.store, datboi_exec::ExecConfig::default()).expect("executor");
    assert!(
        exec.open_stream(&w.db, container).is_err(),
        "the archive is not reconstructible — that is the point"
    );
    // The members still read, straight off their literals.
    for want in members {
        let hash = Blake3::compute(want);
        let got = exec
            .serve_range(&w.db, &hash, 0, want.len() as u64)
            .expect("member serves");
        assert_eq!(&got, want);
    }
}

#[test]
fn unpack_at_ingest_makes_members_resident_and_drops_the_archive() {
    let mut w = world();
    let (zip, stored, deflated) = two_member_zip();
    let path = w.dir.path().join("game.zip");
    fs::write(&path, &zip).expect("write");
    let container = Blake3::compute(&zip);

    let report = Ingester::new(&w.store, &mut w.db, &[])
        .with_config(IngestConfig {
            unpack: true,
            ..IngestConfig::default()
        })
        .ingest(&[&path]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.containers_unpacked, 1);
    assert_eq!(report.container_bytes_dropped, zip.len() as u64);
    assert_eq!(report.members_claimed, 2);

    assert_unpacked(&w, &container, &[&stored, &deflated], "game.zip");
}

#[test]
fn the_pass_converts_a_corpus_ingested_under_the_retaining_default() {
    let mut w = world();
    let (zip, stored, deflated) = two_member_zip();
    let path = w.dir.path().join("game.zip");
    fs::write(&path, &zip).expect("write");
    let container = Blake3::compute(&zip);

    // The retaining default: container literal, members claimed.
    let report = Ingester::new(&w.store, &mut w.db, &[]).ingest(&[&path]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(w.store.has(StoreNs::Data, &container));
    for member in [&stored, &deflated] {
        let hash = Blake3::compute(member);
        assert!(
            !w.store.has(StoreNs::Data, &hash),
            "a claimed member has no bytes of its own"
        );
    }

    let report = run_pass(&w, &UnpackOptions::default());
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.population, 1);
    assert_eq!(report.examined, 1);
    assert_eq!(report.unpacked, 1);
    assert_eq!(report.members_resident, 2);
    assert_eq!(report.dropped_bytes, zip.len() as u64);
    assert_eq!(
        report.member_bytes,
        (stored.len() + deflated.len()) as u64,
        "the storage trade, both halves, in one report"
    );
    assert_eq!(report.population_after, 0);

    assert_unpacked(&w, &container, &[&stored, &deflated], "game.zip");

    // Re-running is a no-op: the container is no longer a candidate.
    let again = run_pass(&w, &UnpackOptions::default());
    assert!(again.complete());
    assert_eq!(again.population, 0);
    assert_eq!(again.examined, 0);
    assert_eq!(again.unpacked, 0);
}

#[test]
fn a_dry_run_states_both_halves_of_the_bill_and_destroys_nothing() {
    let mut w = world();
    let (zip, stored, deflated) = two_member_zip();
    let path = w.dir.path().join("game.zip");
    fs::write(&path, &zip).expect("write");
    let container = Blake3::compute(&zip);
    Ingester::new(&w.store, &mut w.db, &[]).ingest(&[&path]);

    let report = run_pass(
        &w,
        &UnpackOptions {
            dry_run: true,
            ..UnpackOptions::default()
        },
    );
    assert_eq!(report.selected, 1);
    assert_eq!(report.selected_bytes, zip.len() as u64);
    assert_eq!(
        report.claimed_member_bytes,
        (stored.len() + deflated.len()) as u64
    );
    assert_eq!(report.unpacked, 0);
    assert!(
        !report.complete(),
        "a dry run with work to do is the incomplete exit code"
    );
    assert!(w.store.has(StoreNs::Data, &container), "nothing destroyed");
}

#[test]
fn the_pass_converts_7z_and_rar_the_same_way_it_converts_zip() {
    let mut w = world();
    let rom_a = pattern(300_000, 0xAAAA_BBBB_CCCC_DDDD);
    let rom_b = b"tiny member".to_vec();
    let archive = w.dir.path().join("set.7z");
    let mut writer = sevenz_rust2::ArchiveWriter::create(&archive).expect("writer");
    writer
        .push_archive_entry(
            sevenz_rust2::ArchiveEntry::new_file("a.rom"),
            Some(rom_a.as_slice()),
        )
        .expect("entry a");
    writer
        .push_archive_entry(
            sevenz_rust2::ArchiveEntry::new_file("b.rom"),
            Some(rom_b.as_slice()),
        )
        .expect("entry b");
    writer.finish().expect("finish");
    let container = Blake3::compute(&fs::read(&archive).expect("read"));

    let report = Ingester::new(&w.store, &mut w.db, &[]).ingest(&[&archive]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.members_extracted, 2);
    // The retaining default's 2x: container AND members, both resident.
    assert!(w.store.has(StoreNs::Data, &container));
    assert!(w.store.has(StoreNs::Data, &Blake3::compute(&rom_a)));

    let report = run_pass(&w, &UnpackOptions::default());
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.unpacked, 1);
    assert_unpacked(&w, &container, &[&rom_a, &rom_b], "set.7z");
}

#[test]
fn rar_unpacks_at_the_door_like_every_other_transport() {
    let mut w = world();
    // The committed fixture (rar cannot be created programmatically —
    // extraction-only by license); one member, "VERSION".
    let rar = include_bytes!("fixtures/version.rar");
    let path = w.dir.path().join("version.rar");
    fs::write(&path, rar).expect("write fixture");
    let container = Blake3::compute(rar);

    let report = Ingester::new(&w.store, &mut w.db, &[])
        .with_config(IngestConfig {
            unpack: true,
            ..IngestConfig::default()
        })
        .ingest(&[&path]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.members_extracted, 1);
    assert_eq!(report.containers_unpacked, 1);
    assert_unpacked(&w, &container, &[b"unrar-0.4.0"], "version.rar");
}

#[test]
fn a_container_that_would_not_give_up_every_member_is_kept_whole() {
    let mut w = world();
    let good = pattern(20_000, 0xFEED_FACE_CAFE_BEEF);
    let mut zb = ZipBuilder::new();
    zb.add("good.rom", &good, true);
    // Encrypted: our parser refuses it, so those bytes exist nowhere
    // else and the archive is the only copy. Dropping it would be loss.
    zb.add_raw("secret.rom", b"unreadable", 0, 0x0001);
    let zip = zb.finish();
    let path = w.dir.path().join("mixed.zip");
    fs::write(&path, &zip).expect("write");
    let container = Blake3::compute(&zip);

    let report = Ingester::new(&w.store, &mut w.db, &[])
        .with_config(IngestConfig {
            unpack: true,
            ..IngestConfig::default()
        })
        .ingest(&[&path]);
    assert_eq!(report.member_skips.len(), 1, "the encrypted member");
    assert_eq!(report.containers_unpacked, 0);
    assert!(
        w.store.has(StoreNs::Data, &container),
        "a container with a member we cannot read is kept whole"
    );
    // The member we COULD read is resident either way.
    assert_eq!(&bytes_of(&w.store, &Blake3::compute(&good)), &good);
}

#[test]
fn an_interrupt_before_the_index_rows_leaves_members_derivable() {
    let mut w = world();
    let (zip, stored, deflated) = two_member_zip();
    let path = w.dir.path().join("game.zip");
    fs::write(&path, &zip).expect("write");
    let container = Blake3::compute(&zip);
    Ingester::new(&w.store, &mut w.db, &[]).ingest(&[&path]);

    // The deep window: a worker published member bytes and died before
    // the coordinator could write a single row. D122's SAFE drift.
    datboi_ingest::unpack::zip_members_into_store(&w.store, &container).expect("extract");
    for member in [&stored, &deflated] {
        let hash = Blake3::compute(member);
        assert!(w.store.has(StoreNs::Data, &hash), "bytes are durable");
        assert_eq!(
            w.db.blob_by_hash(&hash).expect("q").expect("row").residency,
            Residency::Absent,
            "the index has not caught up"
        );
    }
    // The container is untouched, so every member is still derivable
    // through its recipe — nothing is lost, whatever happens next.
    assert!(w.store.has(StoreNs::Data, &container));

    // A re-run converges.
    let report = run_pass(&w, &UnpackOptions::default());
    assert!(report.complete(), "{report:?}");
    assert_unpacked(&w, &container, &[&stored, &deflated], "game.zip");
}

#[test]
fn an_interrupt_between_the_unlink_and_the_residency_flip_converges() {
    let mut w = world();
    let (zip, stored, deflated) = two_member_zip();
    let path = w.dir.path().join("game.zip");
    fs::write(&path, &zip).expect("write");
    let container = Blake3::compute(&zip);
    Ingester::new(&w.store, &mut w.db, &[]).ingest(&[&path]);

    // Members durable and recorded, container's file gone, row still
    // claiming Resident — the one window two media cannot close.
    let (members, skips) =
        datboi_ingest::unpack::zip_members_into_store(&w.store, &container).expect("extract");
    assert!(skips.is_empty());
    for member in &members {
        datboi_ingest::unpack::record_member(&w.db, member).expect("record");
    }
    w.store
        .remove_blob(StoreNs::Data, &container)
        .expect("unlink");
    assert_eq!(
        w.db.blob_by_hash(&container)
            .expect("q")
            .expect("row")
            .residency,
        Residency::Resident,
        "the row is stale, by construction"
    );

    let report = run_pass(&w, &UnpackOptions::default());
    assert!(report.complete(), "{report:?}");
    assert_eq!(
        report.reconciled, 1,
        "the pass finished the interrupted flip"
    );
    assert_eq!(report.unpacked, 0, "there was nothing left to extract");
    assert_unpacked(&w, &container, &[&stored, &deflated], "game.zip");
}

#[test]
fn a_limit_stops_short_and_a_re_run_finishes() {
    let mut w = world();
    let mut containers = Vec::new();
    for i in 0..3u8 {
        let rom = pattern(30_000, 0x1111_2222_3333_0000 + u64::from(i));
        let mut zb = ZipBuilder::new();
        zb.add("rom.bin", &rom, true);
        let zip = zb.finish();
        fs::write(w.dir.path().join(format!("{i}.zip")), &zip).expect("write");
        containers.push(Blake3::compute(&zip));
    }
    let dir = w.dir.path().to_owned();
    Ingester::new(&w.store, &mut w.db, &[]).ingest(&[&dir]);

    let first = run_pass(
        &w,
        &UnpackOptions {
            limit: 2,
            ..UnpackOptions::default()
        },
    );
    assert_eq!(first.unpacked, 2);
    assert!(
        !first.complete(),
        "a walk cut short never claims completion"
    );
    assert_eq!(first.population_after, 1);

    let second = run_pass(&w, &UnpackOptions::default());
    assert!(second.complete(), "{second:?}");
    assert_eq!(second.unpacked, 1);
    for container in &containers {
        assert!(!w.store.has(StoreNs::Data, container));
    }
}

#[test]
fn both_doors_converge_on_the_same_graph() {
    let (zip, stored, deflated) = two_member_zip();

    let at_the_door = {
        let mut w = world();
        let path = w.dir.path().join("game.zip");
        fs::write(&path, &zip).expect("write");
        Ingester::new(&w.store, &mut w.db, &[])
            .with_config(IngestConfig {
                unpack: true,
                ..IngestConfig::default()
            })
            .ingest(&[&path]);
        graph(&w)
    };
    let after_the_fact = {
        let mut w = world();
        let path = w.dir.path().join("game.zip");
        fs::write(&path, &zip).expect("write");
        Ingester::new(&w.store, &mut w.db, &[]).ingest(&[&path]);
        run_pass(&w, &UnpackOptions::default());
        graph(&w)
    };
    assert_eq!(
        at_the_door, after_the_fact,
        "`ingest --unpack` and `ingest` + `unpack` are the same end state"
    );
    // And that state is the unpacked one, not the retained one.
    assert!(at_the_door.contains(&(
        Blake3::compute(&stored).to_hex(),
        Residency::Resident as i64,
    )));
    assert!(at_the_door.contains(&(
        Blake3::compute(&deflated).to_hex(),
        Residency::Resident as i64,
    )));
    assert!(at_the_door.contains(&(Blake3::compute(&zip).to_hex(), Residency::Absent as i64)));
}

/// Every Data blob's (hash, residency) plus every recipe's op name —
/// enough to tell two graphs apart without depending on row ids.
fn graph(w: &World) -> Vec<(String, i64)> {
    let mut rows: Vec<(String, i64)> =
        w.db.cache()
            .prepare("SELECT hash, residency FROM blob WHERE namespace = 0")
            .expect("prepare")
            .query_map([], |r| {
                Ok((
                    Blake3(r.get::<_, [u8; 32]>(0)?).to_hex(),
                    r.get::<_, i64>(1)?,
                ))
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
    let mut names: Vec<(String, i64)> =
        w.db.cache()
            .prepare("SELECT op_name, COUNT(*) FROM recipe GROUP BY op_name")
            .expect("prepare")
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
    rows.append(&mut names);
    rows.sort();
    rows
}

impl World {
    fn workers(&self) -> usize {
        UnpackOptions::default().workers()
    }
}

fn run_pass(w: &World, opts: &UnpackOptions) -> UnpackReport {
    unpack_corpus(&w.store, &w.db, opts, &mut |_| {}).expect("pass")
}

// ---- D123's measurement ----

/// The before/after D123 was ruled on, reproducible on any machine.
///
/// Two numbers, and they point in opposite directions: unpacking a rom
/// corpus COSTS storage (members are plaintext, the archive was not)
/// and BUYS reads (an Opaque route re-materializes the whole member for
/// every window `produce_range` asks for, which is quadratic in member
/// size; a resident literal is a plain file read).
///
/// Ignored by default — it writes a few hundred MB and the read half is
/// meaningless in a debug build. Run it as:
/// `cargo test --release -p datboi-ingest --test unpack -- --ignored --nocapture`
#[test]
#[ignore = "storage + read-latency measurement; use --release --nocapture"]
fn measure_the_unpack_trade() {
    use std::time::Instant;

    const ZIPS: usize = 40;
    const PER_ZIP: usize = 4;
    const MEMBER_BYTES: usize = 2 << 20;
    /// The window an NFS read arrives in.
    const WINDOW: u64 = 128 << 10;
    const BIG_MEMBER: usize = 16 << 20;

    let mut w = world();
    let src = w.dir.path().join("src");
    fs::create_dir_all(&src).expect("src");

    // Roms are neither random nor text: ~2:1 through DEFLATE, which is
    // the ratio the MAME set shows.
    let romish = |seed: u64, len: usize| -> Vec<u8> {
        let noise = pattern(len / 2, seed);
        let mut out = Vec::with_capacity(len);
        for byte in noise {
            out.push(byte);
            out.push(0);
        }
        out.resize(len, 0);
        out
    };

    for z in 0..ZIPS {
        let mut zb = ZipBuilder::new();
        for m in 0..PER_ZIP {
            let body = romish(
                0x5EED_0000_0000_0000 + (z * PER_ZIP + m) as u64,
                MEMBER_BYTES,
            );
            zb.add(&format!("rom{m}.bin"), &body, true);
        }
        fs::write(src.join(format!("set{z}.zip")), zb.finish()).expect("zip");
    }
    // One big member on its own, for the read half.
    let big = romish(0xB16B_00B5_0000_0001, BIG_MEMBER);
    let mut zb = ZipBuilder::new();
    zb.add("big.bin", &big, true);
    fs::write(src.join("big.zip"), zb.finish()).expect("big zip");
    let big_hash = Blake3::compute(&big);

    let t0 = Instant::now();
    let report = Ingester::new(&w.store, &mut w.db, &[]).ingest(&[&src]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let ingest_secs = t0.elapsed().as_secs_f64();
    let before = store_bytes(w.dir.path().join("store"));

    // The read tax, measured the way a client pays it: whole member,
    // one NFS window at a time, through the Opaque route.
    let exec =
        datboi_exec::Executor::new(&w.store, datboi_exec::ExecConfig::default()).expect("executor");
    let windowed = |exec: &datboi_exec::Executor<'_>, db: &Db| -> f64 {
        let t = Instant::now();
        let mut offset = 0u64;
        while offset < BIG_MEMBER as u64 {
            exec.serve_range(db, &big_hash, offset, WINDOW)
                .expect("serve");
            offset += WINDOW;
        }
        t.elapsed().as_secs_f64()
    };
    let serial_read = windowed(&exec, &w.db);

    let t1 = Instant::now();
    let pass = run_pass(&w, &UnpackOptions::default());
    let pass_secs = t1.elapsed().as_secs_f64();
    assert!(pass.complete(), "{pass:?}");
    let after = store_bytes(w.dir.path().join("store"));
    let unpacked_read = windowed(&exec, &w.db);

    let windows = BIG_MEMBER as u64 / WINDOW;
    println!("\n=== D123: the unpack trade, {} threads ===", w.workers());
    println!(
        "corpus            {ZIPS} zips x {PER_ZIP} members x {} + one {} member",
        human(MEMBER_BYTES as u64),
        human(BIG_MEMBER as u64),
    );
    println!("ingest            {ingest_secs:.2}s");
    println!(
        "unpack            {pass_secs:.2}s, {} containers",
        pass.unpacked
    );
    println!("store before      {} ({} bytes)", human(before), before);
    println!("store after       {} ({} bytes)", human(after), after);
    println!(
        "storage delta     {:+.1}%  (-{} of archives, +{} of members)",
        (after as f64 - before as f64) / before as f64 * 100.0,
        human(pass.dropped_bytes),
        human(pass.member_bytes),
    );
    println!(
        "read {} in {windows} x {} windows:",
        human(BIG_MEMBER as u64),
        human(WINDOW)
    );
    println!("  container route {serial_read:.2}s   (Opaque: one full inflate per window)");
    println!("  unpacked        {unpacked_read:.3}s   (resident literal: a plain file read)");
    println!(
        "  speedup         {:.0}x",
        serial_read / unpacked_read.max(1e-9)
    );

    assert!(
        unpacked_read < serial_read,
        "unpacking must not make reads slower"
    );
}

fn store_bytes(root: std::path::PathBuf) -> u64 {
    let mut total = 0;
    let mut dirs = vec![root];
    while let Some(dir) = dirs.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}
