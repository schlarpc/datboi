//! The D121 bulk blessing pass over a corpus of zipped DEFLATE members
//! — the shape the live deployment is in: 107,090 members over one bao
//! group, no sidecar, no carve-out, every first read a full inflate.
//!
//! Every test starts from an ingested corpus with its sidecars deleted.
//! That is not a contrivance: ingest builds the tree in the same inflate
//! that builds the alias tuple (D63 amendment A), so a corpus adopted
//! TODAY needs no pass — and the corpus that needs one is exactly the
//! one adopted before that landed, which is the sidecar-less state this
//! re-creates.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use datboi_core::hash::Blake3;
use datboi_exec::bless::{BlessOptions, BlessReport};
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Residency, VerifyState};
use datboi_ingest::Ingester;
use datboi_store_fs::{Namespace as StoreNs, Store, obao};
use flate2::Compression;
use flate2::write::DeflateEncoder;

/// Past one bao group by enough that the tree has interior nodes.
const BIG_LEN: usize = 300_000;
/// Zips in the fixture corpus, and deflated members in each.
const ZIPS: usize = 6;
const MEMBERS: usize = 4;

/// Deterministic and DEFLATE-friendly: a pseudo-random 1 KiB tile
/// salted per 64 KiB, so it compresses without being one repeated run.
fn pattern(len: usize, salt: u16) -> Vec<u8> {
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
        .map(|i| tile[i % 1024] ^ ((i >> 16) as u8) ^ (salt as u8))
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
    /// Every deflated member's plaintext, in fixture order.
    deflated: Vec<Vec<u8>>,
    /// Every STORED member's plaintext — the D63 carve-out lane.
    stored: Vec<Vec<u8>>,
}

impl World {
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

    fn store_root(&self) -> std::path::PathBuf {
        self.dir.path().join("store")
    }

    /// Every sidecar in the store, keyed by its path relative to the
    /// store root — the "identical store contents" oracle.
    fn sidecars(&self) -> BTreeMap<String, Vec<u8>> {
        let root = self.store_root();
        let mut out = BTreeMap::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "obao4") {
                    let rel = path
                        .strip_prefix(&root)
                        .expect("under the store")
                        .to_string_lossy()
                        .into_owned();
                    out.insert(rel, fs::read(&path).expect("read sidecar"));
                }
            }
        }
        out
    }

    fn bless(&self, opts: &BlessOptions) -> BlessReport {
        self.exec()
            .bless_corpus(&self.db, opts, &mut |_| {})
            .expect("bless pass")
    }
}

/// The corpus: `ZIPS` zips of `MEMBERS` deflated members plus one
/// STORED member each, ingested, then stripped of every sidecar.
fn corpus() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    let mut deflated = Vec::new();
    let mut stored = Vec::new();
    let mut paths = Vec::new();
    for z in 0..ZIPS {
        let mut zb = ZipBuilder::default();
        for m in 0..MEMBERS {
            let salt = u16::try_from(z * 16 + m).expect("small");
            let bytes = pattern(BIG_LEN + m * 7919, salt);
            zb.add(&format!("rom{m}.bin"), &bytes, true);
            deflated.push(bytes);
        }
        let raw = pattern(BIG_LEN, u16::try_from(1000 + z).expect("small"));
        zb.add("raw.bin", &raw, false);
        stored.push(raw);

        let path = dir.path().join(format!("set{z}.zip"));
        fs::write(&path, zb.finish()).expect("zip");
        paths.push(path);
    }

    let report = Ingester::new(&store, &mut db, &[]).ingest(&paths);
    assert_eq!(report.errors, vec![], "{:?}", report.errors);
    assert_eq!(report.members_claimed as usize, ZIPS * (MEMBERS + 1));

    let world = World {
        dir,
        store,
        db,
        deflated,
        stored,
    };
    // The pre-amendment state: claims and routes exist, member bytes do
    // not, and no tree was ever built.
    unbless(&world.store_root());
    assert!(world.sidecars().is_empty(), "every tree removed");
    world
}

fn unbless(root: &Path) {
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "obao4") {
                fs::remove_file(&path).expect("remove sidecar");
            }
        }
    }
}

/// The corpus-sized claim: every deflate member comes out with a tree
/// whose bytes are the tree of the member's bytes, and whose root is
/// the hash the claim names.
#[test]
fn the_pass_blesses_every_member_and_the_roots_match_the_claims() {
    let w = corpus();
    let report = w.bless(&BlessOptions {
        parallelism: 4,
        ..BlessOptions::default()
    });

    assert_eq!(report.failed, Vec::<(String, String)>::new());
    assert_eq!(report.blessed as usize, ZIPS * MEMBERS);
    assert_eq!(report.selected, report.blessed);
    assert_eq!(report.already_blessed, 0);
    assert_eq!(
        report.carved_out as usize, ZIPS,
        "the STORED member of each zip is the carve-out's, not ours"
    );
    assert_eq!(
        report.bytes,
        w.deflated.iter().map(|b| b.len() as u64).sum::<u64>()
    );

    for bytes in &w.deflated {
        let hash = Blake3::compute(bytes);
        let (root, want) = obao::compute(&bytes[..], bytes.len() as u64).expect("compute");
        assert_eq!(root, hash, "the obao root IS the claim");
        assert_eq!(
            w.store.get_obao(StoreNs::Data, &hash).expect("q"),
            Some(want),
            "the pass wrote the same tree a direct computation would"
        );
        // The member bytes are still absent — a tree with no `.data`
        // beside it is the D49 rule-1 shape, and the whole point: the
        // pass buys readability, not 143 GB of residency.
        assert!(!w.store.has(StoreNs::Data, &hash));
    }

    // And the bytes read back correctly off the fresh trees.
    let exec = w.exec();
    for bytes in &w.deflated {
        let hash = Blake3::compute(bytes);
        let at = (bytes.len() / 3) as u64;
        let got = exec.serve_range(&w.db, &hash, at, 9_000).expect("range");
        let lo = usize::try_from(at).expect("fits");
        assert_eq!(got, &bytes[lo..lo + 9_000]);
    }
}

/// Idempotence: the sidecar IS the checkpoint, so a second run costs
/// stats and nothing else.
#[test]
fn rerunning_the_pass_does_no_work() {
    let w = corpus();
    let first = w.bless(&BlessOptions::default());
    assert_eq!(first.blessed as usize, ZIPS * MEMBERS);
    let before = w.sidecars();

    let second = w.bless(&BlessOptions::default());
    assert_eq!(second.blessed, 0, "nothing left to do");
    assert_eq!(second.selected, 0);
    assert_eq!(second.bytes, 0);
    assert_eq!(second.already_blessed as usize, ZIPS * MEMBERS);
    assert_eq!(second.examined, first.examined, "same candidate set");
    assert_eq!(w.sidecars(), before, "and it touched nothing");
}

/// Interruption: `--limit` cuts a run short exactly as a Ctrl-C would
/// (the pass holds no state either way), and resuming converges on the
/// same store an uninterrupted run produces.
#[test]
fn an_interrupted_pass_resumes_to_the_same_state() {
    let piecemeal = corpus();
    let mut rounds = 0;
    loop {
        let report = piecemeal.bless(&BlessOptions {
            limit: 5,
            parallelism: 3,
            ..BlessOptions::default()
        });
        assert_eq!(report.failed, Vec::<(String, String)>::new());
        if report.selected == 0 {
            break;
        }
        assert!(report.selected <= 5, "the limit holds");
        rounds += 1;
        assert!(rounds < 20, "converging, not looping");
    }
    assert!(rounds > 1, "the corpus really did take several rounds");

    let whole = corpus();
    whole.bless(&BlessOptions::default());
    assert_eq!(
        piecemeal.sidecars(),
        whole.sidecars(),
        "interrupted-and-resumed lands on the uninterrupted store"
    );
}

/// D93's posture in one assertion: parallelism is scheduling, not
/// semantics. Eight workers and one must leave byte-identical stores.
#[test]
fn parallel_and_serial_runs_produce_identical_store_contents() {
    let serial = corpus();
    let parallel = corpus();
    assert_eq!(
        serial.sidecars(),
        parallel.sidecars(),
        "the two fixtures start identical"
    );

    let a = serial.bless(&BlessOptions {
        parallelism: 1,
        ..BlessOptions::default()
    });
    let b = parallel.bless(&BlessOptions {
        parallelism: 8,
        ..BlessOptions::default()
    });

    assert_eq!(serial.sidecars(), parallel.sidecars());
    assert_eq!((a.blessed, a.bytes), (b.blessed, b.bytes));
    assert_eq!(a.examined, b.examined);
    assert_eq!(a.failed, b.failed);
}

/// The D121 selection ruling: the default blesses what the carve-out
/// CANNOT serve, and D63's literal sentence — promote a carved-out
/// route to full D49 — is the opt-in.
#[test]
fn the_carve_out_is_skipped_by_default_and_promoted_on_request() {
    let w = corpus();
    let default_run = w.bless(&BlessOptions::default());
    assert_eq!(default_run.carved_out as usize, ZIPS);
    for bytes in &w.stored {
        let hash = Blake3::compute(bytes);
        assert_eq!(
            w.store.get_obao(StoreNs::Data, &hash).expect("q"),
            None,
            "a STORED member serves off the carve-out; D63 refuses the pass here"
        );
        // Unserved is the thing that would be a bug — it reads fine.
        let got = w
            .exec()
            .serve_range(&w.db, &hash, 20_000, 4_096)
            .expect("carve-out range");
        assert_eq!(got, &bytes[20_000..24_096]);
    }

    let promote = w.bless(&BlessOptions {
        include_affine: true,
        ..BlessOptions::default()
    });
    assert_eq!(promote.carved_out, 0, "nothing is skipped for affinity now");
    assert_eq!(
        promote.blessed as usize, ZIPS,
        "the STORED members, promoted"
    );
    for bytes in &w.stored {
        let hash = Blake3::compute(bytes);
        let (_, want) = obao::compute(&bytes[..], bytes.len() as u64).expect("compute");
        assert_eq!(
            w.store.get_obao(StoreNs::Data, &hash).expect("q"),
            Some(want),
            "the floor traded up to a ceiling"
        );
    }
}

/// `--dry-run` is the "how much is outstanding?" answer, and it earns
/// that name: it decides everything a real run decides and writes
/// nothing.
#[test]
fn a_dry_run_counts_the_work_and_writes_nothing() {
    let w = corpus();
    let dry = w.bless(&BlessOptions {
        dry_run: true,
        ..BlessOptions::default()
    });
    assert_eq!(dry.selected as usize, ZIPS * MEMBERS);
    assert_eq!(
        dry.selected_bytes,
        w.deflated.iter().map(|b| b.len() as u64).sum::<u64>()
    );
    assert_eq!(dry.blessed, 0);
    assert_eq!(dry.outstanding(), dry.selected);
    assert!(w.sidecars().is_empty(), "a dry run stores nothing");

    // And it predicted the real run exactly.
    let wet = w.bless(&BlessOptions::default());
    assert_eq!(
        (wet.selected, wet.selected_bytes),
        (dry.selected, dry.selected_bytes)
    );
    assert_eq!(wet.blessed, dry.selected);
}

/// Progress is reported as it happens, not as one silent block: the
/// callback fires per retired candidate and the counters only grow.
#[test]
fn progress_is_reported_per_candidate() {
    let w = corpus();
    let exec = w.exec();
    let mut ticks = 0u64;
    let mut last = 0u64;
    let report = exec
        .bless_corpus(
            &w.db,
            &BlessOptions {
                parallelism: 4,
                ..BlessOptions::default()
            },
            &mut |r| {
                assert!(r.blessed >= last, "counters never go backwards");
                last = r.blessed;
                ticks += 1;
            },
        )
        .expect("pass");
    assert_eq!(ticks, report.examined, "one tick per candidate retired");
    assert_eq!(last, report.blessed);
}

/// D121's residency ruling: `--materialize` keeps what the pass already
/// inflated. The bytes land resident, the index says so, and the route
/// that produced them is licensed — which is what makes the decision
/// reversible instead of a one-way 143 GB spend.
#[test]
fn materialize_keeps_the_bytes_and_the_index_agrees() {
    let w = corpus();
    let report = w.bless(&BlessOptions {
        materialize: true,
        parallelism: 4,
        ..BlessOptions::default()
    });
    assert_eq!(report.failed, Vec::<(String, String)>::new());
    assert_eq!(report.materialized as usize, ZIPS * MEMBERS);
    assert_eq!(report.blessed, report.materialized);
    assert!(!report.out_of_room);

    for bytes in &w.deflated {
        let hash = Blake3::compute(bytes);
        // The bytes are here, and they are the right bytes.
        assert!(w.store.has(StoreNs::Data, &hash), "kept, not discarded");
        let mut got = Vec::new();
        std::io::Read::read_to_end(
            &mut w
                .store
                .get(StoreNs::Data, &hash)
                .expect("get")
                .expect("resident"),
            &mut got,
        )
        .expect("read");
        assert_eq!(&got, bytes);
        // The tree rode along in the same pass — `put_with_obao`, not a
        // second read.
        let (_, want) = obao::compute(&bytes[..], bytes.len() as u64).expect("compute");
        assert_eq!(
            w.store.get_obao(StoreNs::Data, &hash).expect("q"),
            Some(want)
        );

        // And the index says what the store now holds.
        let row = w.db.blob_by_hash(&hash).expect("q").expect("claimed");
        assert_eq!(row.residency, Residency::Resident);
        assert!(
            w.db.blob_verified_at(row.blob_id).expect("q").is_some(),
            "bytes were hash-checked on the way in"
        );
        // The producing route replayed on this host (D25's licensing
        // event), so the bytes can be given back.
        let recipes = w.db.recipes_for_output(row.blob_id).expect("q");
        assert_eq!(recipes.len(), 1);
        assert_eq!(recipes[0].verify, VerifyState::ReplayedLocal);
        assert!(
            w.db.is_evictable(row.blob_id).expect("q"),
            "materializing must stay reversible through the evict planner"
        );
    }

    // Reads now take the resident path — no route, no spill.
    let exec = w.exec();
    for bytes in &w.deflated {
        let hash = Blake3::compute(bytes);
        let got = exec
            .serve_range(&w.db, &hash, 50_000, 4_096)
            .expect("range");
        assert_eq!(got, &bytes[50_000..54_096]);
    }

    // Idempotent in this mode too.
    let again = w.bless(&BlessOptions {
        materialize: true,
        ..BlessOptions::default()
    });
    assert_eq!(again.selected, 0);
    assert_eq!(again.materialized, 0);
}

/// The default does NOT keep bytes — the storage bill is opt-in.
#[test]
fn blessing_without_the_flag_leaves_the_bytes_absent() {
    let w = corpus();
    let report = w.bless(&BlessOptions::default());
    assert_eq!(report.materialized, 0);
    for bytes in &w.deflated {
        let hash = Blake3::compute(bytes);
        assert!(!w.store.has(StoreNs::Data, &hash));
        assert_eq!(
            w.db.blob_by_hash(&hash)
                .expect("q")
                .expect("claimed")
                .residency,
            Residency::Absent,
        );
    }
}

/// `--min-size` is how `--materialize` gets aimed: the members where an
/// O(n) spill per window actually hurts are the big ones, and the floor
/// is what leaves the rest alone.
#[test]
fn min_size_aims_the_pass() {
    let w = corpus();
    let above = w.bless(&BlessOptions {
        min_size: 10 << 20,
        dry_run: true,
        ..BlessOptions::default()
    });
    assert_eq!(above.examined, 0, "no member is 10 MiB");
    assert_eq!(above.selected, 0);

    let below = w.bless(&BlessOptions {
        min_size: BIG_LEN as u64 + 1,
        dry_run: true,
        ..BlessOptions::default()
    });
    assert_eq!(
        below.selected as usize,
        ZIPS * (MEMBERS - 1),
        "the floor excludes exactly the smallest member of each zip"
    );

    // A floor under one bao group cannot resurrect blobs that have no
    // tree to build: one group is the hard minimum, and the floor is
    // INCLUSIVE, so it is GROUP_BYTES + 1.
    assert_eq!(BlessOptions::default().floor(), obao::GROUP_BYTES + 1);
    assert_eq!(
        BlessOptions {
            min_size: 1,
            ..BlessOptions::default()
        }
        .floor(),
        obao::GROUP_BYTES + 1
    );
}

/// The bug that made `--materialize` skip the members that hurt most.
///
/// A member blessed ON DEMAND by the serve path (D63 amendment) has a
/// sidecar and no bytes. The pass treated "has a sidecar" as "nothing
/// to do" in both modes, so every member a reader had already paid a
/// full materialization for — which is exactly the set a client has
/// proved is painful — was skipped by the one flag that would have
/// fixed it. In materialize mode the goal state is BYTES.
#[test]
fn materialize_does_not_skip_an_already_blessed_but_absent_member() {
    let w = corpus();
    // Bless first: every member now has a tree and no bytes — the state
    // a `mame -verifyroms` leaves behind.
    let blessed = w.bless(&BlessOptions::default());
    assert_eq!(blessed.blessed as usize, ZIPS * MEMBERS);
    for bytes in &w.deflated {
        let hash = Blake3::compute(bytes);
        assert!(w.store.has_obao(StoreNs::Data, &hash).expect("q"));
        assert!(!w.store.has(StoreNs::Data, &hash), "tree, but no bytes");
    }

    // Now materialize. Every one of them is still outstanding work.
    let report = w.bless(&BlessOptions {
        materialize: true,
        parallelism: 4,
        ..BlessOptions::default()
    });
    assert_eq!(report.failed, Vec::<(String, String)>::new());
    assert_eq!(
        report.already_blessed, 0,
        "a sidecar is not a materialization"
    );
    assert_eq!(report.selected as usize, ZIPS * MEMBERS);
    assert_eq!(report.materialized as usize, ZIPS * MEMBERS);
    assert!(report.complete());
    for bytes in &w.deflated {
        let hash = Blake3::compute(bytes);
        assert!(w.store.has(StoreNs::Data, &hash), "bytes are here now");
        assert_eq!(
            w.db.blob_by_hash(&hash).expect("q").expect("row").residency,
            Residency::Resident,
        );
    }

    // And NOW it is a no-op, because the bytes are what it checks.
    let again = w.bless(&BlessOptions {
        materialize: true,
        ..BlessOptions::default()
    });
    assert_eq!(again.selected, 0);
    assert_eq!(again.materialized, 0);
    // What remains in the population is the STORED member of each zip:
    // still Absent, and skipped for the carve-out, not for a sidecar.
    // The population is a candidate count, never a progress bar.
    assert_eq!(again.population as usize, ZIPS);
    assert_eq!(again.carved_out as usize, ZIPS);
    assert!(again.complete());
}

/// "Nothing outstanding" has to be a CHECKED claim, not an inference
/// from the pass's own counters: the pass reports the population a
/// straight SQL count gave it, and whether the walk covered all of it.
#[test]
fn the_pass_reports_the_population_it_was_given() {
    let w = corpus();
    let expected =
        w.db.bless_candidate_count(obao::GROUP_BYTES + 1)
            .expect("count");
    assert_eq!(
        expected as usize,
        ZIPS * (MEMBERS + 1),
        "deflate members plus the STORED one from each zip"
    );

    // A dry run and a real run must agree on the population, and both
    // must agree with the count.
    let dry = w.bless(&BlessOptions {
        dry_run: true,
        ..BlessOptions::default()
    });
    assert_eq!(dry.population, expected);
    assert_eq!(dry.examined, expected);
    assert!(dry.walked_it_all());
    assert!(!dry.complete(), "a dry run with work to do is not complete");

    let wet = w.bless(&BlessOptions::default());
    assert_eq!(wet.population, dry.population);
    assert_eq!(wet.examined, dry.examined);
    assert_eq!(wet.selected, dry.selected);
    assert!(wet.complete());
    // Blessing changes no residency, so the population is untouched —
    // the sidecar is the record, and SQL cannot see it. Saying so is
    // the point: the count is not a progress bar.
    assert_eq!(wet.population_after, expected);

    // Materializing DOES shrink the population, and the after-count is
    // how that is shown rather than claimed.
    let mat = w.bless(&BlessOptions {
        materialize: true,
        ..BlessOptions::default()
    });
    assert_eq!(mat.population, expected);
    // NOT zero: the STORED member of each zip is a candidate the
    // carve-out declines, so it stays Absent forever. `complete()` is
    // therefore defined on the WALK (examined == population) and the
    // work (`outstanding`), never on the population emptying — a corpus
    // where it emptied would be one where the carve-out did not exist.
    assert_eq!(mat.population_after as usize, ZIPS);
    assert!(mat.complete());
}

/// The paging half of the same bug report: a population several times
/// the page size must be walked in full. Built out of index rows alone
/// (every route is ungroundable, so every candidate lands in
/// `no_route`) — what is under test is the walk, not the inflate.
#[test]
fn the_walk_covers_a_population_many_pages_deep() {
    use datboi_index::{Namespace as IndexNs, recipes::NewRecipe};

    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    // Comfortably past the pass's 4096-row page, and every candidate
    // the same size so a boundary has ties on both sides of it.
    const N: usize = 9_001;
    const SIZE: u64 = 64 * 1024;
    let missing = Blake3::compute(b"an input nothing has");
    let input = db
        .upsert_blob(&missing, Some(SIZE), IndexNs::Data, Residency::Absent)
        .expect("upsert");
    for i in 0..N {
        let out = db
            .upsert_blob(
                &Blake3::compute(format!("member-{i:05}").as_bytes()),
                Some(SIZE),
                IndexNs::Data,
                Residency::Absent,
            )
            .expect("upsert");
        let meta = db
            .upsert_blob(
                &Blake3::compute(format!("recipe-{i:05}").as_bytes()),
                Some(128),
                IndexNs::Meta,
                Residency::Resident,
            )
            .expect("upsert");
        db.insert_recipe(&NewRecipe {
            blob_id: meta,
            op_kind: datboi_index::OpKind::Builtin,
            op_name: "assemble@1",
            seek_class: datboi_index::SeekClass::Affine,
            source: datboi_index::RecipeSource::LocalIngest,
            inputs: &[(0, input, None)],
            outputs: &[(0, out, SIZE, None)],
        })
        .expect("insert recipe");
    }
    // The ungroundable input is itself a candidate-shaped row; the
    // count twin is the authority on the population either way.
    let expected = db
        .bless_candidate_count(obao::GROUP_BYTES + 1)
        .expect("count");
    assert_eq!(
        expected as usize, N,
        "N members, the input has no route row"
    );

    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let report = exec
        .bless_corpus(&db, &BlessOptions::default(), &mut |_| {})
        .expect("pass");
    assert_eq!(report.population, expected);
    assert_eq!(
        report.examined, expected,
        "every candidate past the page boundary was visited"
    );
    assert_eq!(report.no_route, expected);
    assert!(report.walked_it_all());
    assert_eq!(report.failed, Vec::<(String, String)>::new());
}
