//! End-to-end catalog tests: unification matrix, six-state audit against
//! a real ingest, D38 demotion, dir2dat round-trip.

use datboi_catalog::{ImportOptions, audit, export_dat, import_dat, refresh_rollups, relink_all};
use datboi_core::alias::AliasHasher;
use datboi_formats::parse;
use datboi_index::Db;
use datboi_ingest::Ingester;
use datboi_store_fs::Store;
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    store: Store,
    db: Db,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    std::fs::create_dir_all(dir.path().join("db")).expect("db dir");
    let db = Db::open(&dir.path().join("db")).expect("db");
    Fixture {
        _dir: dir,
        store,
        db,
    }
}

fn opts(provider: &'static str, system: &'static str) -> ImportOptions<'static> {
    ImportOptions {
        provider: Some(provider),
        system: Some(system),
        imported_at: 1,
    }
}

struct Hashes {
    size: usize,
    crc: String,
    sha1: String,
}

fn hashes(data: &[u8]) -> Hashes {
    let mut hasher = AliasHasher::new();
    hasher.update(data);
    let tuple = hasher.finalize();
    Hashes {
        size: data.len(),
        crc: hex(&tuple.crc32),
        sha1: hex(&tuple.sha1),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn dat(games: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<!DOCTYPE datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd">
<datafile><header><name>Test</name><description>Test dat</description><version>1</version><author>tester</author></header>{games}</datafile>"#
    )
}

/// Minimal STORED-method zip containing one member.
fn stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
    let mut hasher = AliasHasher::new();
    hasher.update(data);
    let crc = u32::from_be_bytes(hasher.finalize().crc32);
    let (nlen, dlen) = (name.len() as u16, data.len() as u32);

    let mut out = Vec::new();
    // local file header
    out.extend_from_slice(b"PK\x03\x04");
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
    out.extend_from_slice(&0u32.to_le_bytes()); // time+date
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&dlen.to_le_bytes()); // csize
    out.extend_from_slice(&dlen.to_le_bytes()); // usize
    out.extend_from_slice(&nlen.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra len
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(data);

    let cd_offset = out.len() as u32;
    // central directory entry
    out.extend_from_slice(b"PK\x01\x02");
    out.extend_from_slice(&20u16.to_le_bytes()); // version made by
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&0u16.to_le_bytes()); // method
    out.extend_from_slice(&0u32.to_le_bytes()); // time+date
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&dlen.to_le_bytes());
    out.extend_from_slice(&dlen.to_le_bytes());
    out.extend_from_slice(&nlen.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra
    out.extend_from_slice(&0u16.to_le_bytes()); // comment
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
    out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
    out.extend_from_slice(&0u32.to_le_bytes()); // local header offset
    out.extend_from_slice(name.as_bytes());
    let cd_size = out.len() as u32 - cd_offset;

    // EOCD
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn count(db: &Db, sql: &str) -> i64 {
    db.cache()
        .query_row(sql, [], |row| row.get(0))
        .expect("count query")
}

/// Every regular file under `root`, recursively — for asserting a store
/// tree holds exactly what an import should have left behind.
fn files_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

/// A malformed upload (observed live: a .zip where a dat belongs) must
/// error without a trace — no CAS blob, no staged temp, no index rows.
/// import_dat validates (detect + parse) before it stores, so the orphan
/// a content-addressed store can't account for never comes to exist.
#[test]
fn garbage_import_leaves_no_orphan() {
    let mut f = fixture();
    let store_root = f._dir.path().join("store");

    for garbage in [b"not a dat".to_vec(), stored_zip("game.bin", b"payload")] {
        import_dat(&f.store, &mut f.db, &garbage, &opts("p", "s"))
            .expect_err("unparseable bytes must not import");
    }

    assert_eq!(count(&f.db, "SELECT COUNT(*) FROM blob"), 0);
    assert_eq!(count(&f.db, "SELECT COUNT(*) FROM dat_revision"), 0);
    let leftovers = files_under(&store_root);
    assert!(leftovers.is_empty(), "orphaned store files: {leftovers:?}");
}

/// The un-overridden provider chain (import.rs `provider_default`),
/// pinned against the real header conventions surveyed 2026-07-11:
/// No-Intro and TOSEC put the org in `<homepage>` and a maintainer
/// credit roll in `<author>`; Redump uses the org for both; FBNeo puts
/// its URL in homepage and the org in author.
#[test]
fn provider_defaults_prefer_the_org_over_the_credit_roll() {
    let mut f = fixture();
    let no_overrides = ImportOptions {
        provider: None,
        system: None,
        imported_at: 1,
    };
    let mut import = |header: &str| {
        let dat = format!(
            r#"<?xml version="1.0"?>
<!DOCTYPE datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd">
<datafile><header>{header}</header><game name="g"><description>g</description><rom name="g.bin" size="1" crc="00000000"/></game></datafile>"#
        );
        let report =
            import_dat(&f.store, &mut f.db, dat.as_bytes(), &no_overrides).expect("import");
        f.db.cache()
            .query_row(
                "SELECT provider, system FROM dat_source WHERE source_id = ?1",
                [report.source_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .expect("source row")
    };
    // No-Intro shape: the contributor roll call is not the provider.
    assert_eq!(
        import(
            "<name>nds</name><author>alice, bob, carol</author>\
             <homepage>No-Intro</homepage><url>https://www.no-intro.org</url>"
        ),
        ("No-Intro".to_owned(), "nds".to_owned())
    );
    // TOSEC shape: per-dat maintainer lists, stable org in homepage.
    assert_eq!(
        import("<name>gb</name><author>Cassiel</author><homepage>TOSEC</homepage>").0,
        "TOSEC"
    );
    // Redump shape: author and homepage agree.
    assert_eq!(
        import("<name>psx</name><author>redump.org</author><homepage>redump.org</homepage>").0,
        "redump.org"
    );
    // FBNeo shape: a URL-shaped homepage yields to the author.
    assert_eq!(
        import(
            "<name>arcade</name><author>FinalBurn Neo</author>\
             <homepage>https://neo-source.com/</homepage>"
        )
        .0,
        "FinalBurn Neo"
    );
    // No homepage → author; neither → unknown.
    assert_eq!(import("<name>x1</name><author>tester</author>").0, "tester");
    assert_eq!(import("<name>x2</name>").0, "unknown");
}

#[test]
fn same_sha1_across_dats_unifies() {
    let mut f = fixture();
    let h = hashes(b"shared-content");
    let game = format!(
        r#"<game name="g"><description>g</description><rom name="g.bin" size="{}" crc="{}" sha1="{}"/></game>"#,
        h.size, h.crc, h.sha1
    );
    import_dat(
        &f.store,
        &mut f.db,
        dat(&game).as_bytes(),
        &opts("p1", "s1"),
    )
    .expect("import 1");
    import_dat(
        &f.store,
        &mut f.db,
        dat(&game).as_bytes(),
        &opts("p2", "s2"),
    )
    .expect("import 2");

    assert_eq!(count(&f.db, "SELECT COUNT(*) FROM content_identity"), 1);
    assert_eq!(
        count(
            &f.db,
            "SELECT COUNT(*) FROM rom_claim WHERE identity_id IS NOT NULL"
        ),
        2
    );
}

#[test]
fn sha1_collision_splits_identities() {
    let mut f = fixture();
    let sha1 = "aa".repeat(20);
    let mk = |md5: &str| {
        dat(&format!(
            r#"<game name="g"><description>g</description><rom name="g.bin" size="8" md5="{md5}" sha1="{sha1}"/></game>"#
        ))
    };
    let a = mk(&"11".repeat(16));
    let b = mk(&"22".repeat(16));
    import_dat(&f.store, &mut f.db, a.as_bytes(), &opts("p1", "s1")).expect("import 1");
    import_dat(&f.store, &mut f.db, b.as_bytes(), &opts("p2", "s2")).expect("import 2");

    // Same sha1, conflicting md5: two identities (sha1 collisions legal, D2).
    assert_eq!(count(&f.db, "SELECT COUNT(*) FROM content_identity"), 2);
}

#[test]
fn crc_only_claims_unify_as_probable() {
    let mut f = fixture();
    let game = r#"<game name="g"><description>g</description><rom name="g.bin" size="4" crc="deadbeef"/></game>"#;
    import_dat(&f.store, &mut f.db, dat(game).as_bytes(), &opts("p1", "s1")).expect("import 1");
    import_dat(&f.store, &mut f.db, dat(game).as_bytes(), &opts("p2", "s2")).expect("import 2");

    assert_eq!(count(&f.db, "SELECT COUNT(*) FROM content_identity"), 1);
    assert_eq!(
        count(
            &f.db,
            "SELECT strength FROM content_identity" // 0 = crc+size (probable)
        ),
        0
    );
}

#[test]
fn zero_byte_rom_shared_by_fifty_entries() {
    let mut f = fixture();
    let h = hashes(b"");
    let games: String = (0..50)
        .map(|i| {
            format!(
                r#"<game name="g{i:02}"><description>g{i}</description><rom name="empty.bin" size="0" crc="{}" sha1="{}"/></game>"#,
                h.crc, h.sha1
            )
        })
        .collect();
    import_dat(&f.store, &mut f.db, dat(&games).as_bytes(), &opts("p", "s")).expect("import");

    assert_eq!(count(&f.db, "SELECT COUNT(*) FROM content_identity"), 1);
    assert_eq!(
        count(
            &f.db,
            "SELECT COUNT(*) FROM rom_claim WHERE identity_id IS NOT NULL"
        ),
        50
    );
}

#[test]
fn audit_six_states_end_to_end() {
    let mut f = fixture();
    let ingest_dir = f._dir.path().join("roms");
    std::fs::create_dir_all(&ingest_dir).expect("roms dir");

    let plain = b"plain-rom-content".as_slice();
    let member = b"member-payload".as_slice();
    let probable_bytes = b"crc-only-rom".as_slice();
    std::fs::write(ingest_dir.join("a.bin"), plain).expect("write a");
    std::fs::write(ingest_dir.join("b.zip"), stored_zip("b.bin", member)).expect("write zip");
    std::fs::write(ingest_dir.join("e.bin"), probable_bytes).expect("write e");

    let report = Ingester::new(&f.store, &mut f.db, &[]).ingest(&[&ingest_dir]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.members_claimed, 1);

    let (ha, hb, he) = (hashes(plain), hashes(member), hashes(probable_bytes));
    let (missing, mia) = (hashes(b"never-ingested"), hashes(b"lost-to-time"));
    let games = format!(
        concat!(
            r#"<game name="alpha"><description>d</description><rom name="a.bin" size="{a_size}" crc="{a_crc}" sha1="{a_sha1}"/></game>"#,
            r#"<game name="beta"><description>d</description><rom name="b.bin" size="{b_size}" crc="{b_crc}" sha1="{b_sha1}"/></game>"#,
            r#"<game name="delta"><description>d</description><rom name="d.bin" status="nodump"/></game>"#,
            r#"<game name="epsilon"><description>d</description><rom name="e.bin" size="{e_size}" crc="{e_crc}"/></game>"#,
            r#"<game name="gamma"><description>d</description><rom name="c.bin" size="{m_size}" sha1="{m_sha1}"/></game>"#,
            r#"<game name="zeta"><description>d</description><rom name="f.bin" size="{z_size}" sha1="{z_sha1}" mia="yes"/></game>"#,
        ),
        a_size = ha.size,
        a_crc = ha.crc,
        a_sha1 = ha.sha1,
        b_size = hb.size,
        b_crc = hb.crc,
        b_sha1 = hb.sha1,
        e_size = he.size,
        e_crc = he.crc,
        m_size = missing.size,
        m_sha1 = missing.sha1,
        z_size = mia.size,
        z_sha1 = mia.sha1,
    );
    import_dat(
        &f.store,
        &mut f.db,
        dat(&games).as_bytes(),
        &opts("no-intro", "Test System"),
    )
    .expect("import");

    let report = audit(&f.db, "no-intro", "Test System").expect("audit");
    let by_name = |name: &str| {
        report
            .entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("entry {name}"))
    };

    // alpha: literal bytes resident → have(verified).
    assert_eq!(by_name("alpha").have_verified, 1);
    // beta: bytes absent, grounded via locally-Verified member recipe →
    // have(verified) per the D4 interpretation (we hashed the real bytes).
    assert_eq!(by_name("beta").have_verified, 1);
    // delta: nodump only → nothing required, trivially complete.
    let delta = by_name("delta");
    assert_eq!((delta.required, delta.missing), (0, 0));
    assert!(delta.complete());
    // epsilon: crc+size-only evidence → probable, not verified (D39).
    let eps = by_name("epsilon");
    assert_eq!((eps.probable, eps.have_verified, eps.missing), (1, 0, 0));
    assert!(!eps.complete());
    // gamma: never ingested → missing.
    assert_eq!(by_name("gamma").missing, 1);
    // zeta: missing and flagged mia upstream.
    let zeta = by_name("zeta");
    assert_eq!((zeta.missing, zeta.mia), (1, 1));

    let t = &report.totals;
    assert_eq!(t.entries, 6);
    assert_eq!(t.entries_complete, 3); // alpha, beta, delta
    assert_eq!(t.have_verified, 2);
    assert_eq!(t.probable, 1);
    assert_eq!(t.missing, 2);
    assert_eq!(t.have_claimed, 0);
    assert_eq!(t.peer_available, 0);
}

#[test]
fn ingest_after_import_needs_relink() {
    let mut f = fixture();
    let content = b"late-arrival".as_slice();
    let h = hashes(content);
    let game = format!(
        r#"<game name="g"><description>d</description><rom name="g.bin" size="{}" crc="{}" sha1="{}"/></game>"#,
        h.size, h.crc, h.sha1
    );
    import_dat(&f.store, &mut f.db, dat(&game).as_bytes(), &opts("p", "s")).expect("import");
    assert_eq!(audit(&f.db, "p", "s").expect("audit").totals.missing, 1);

    let ingest_dir = f._dir.path().join("roms");
    std::fs::create_dir_all(&ingest_dir).expect("dir");
    std::fs::write(ingest_dir.join("g.bin"), content).expect("write");
    let report = Ingester::new(&f.store, &mut f.db, &[]).ingest(&[&ingest_dir]);
    assert!(report.errors.is_empty());

    relink_all(&f.db).expect("relink");
    refresh_rollups(&mut f.db, 2).expect("rollups");
    assert_eq!(
        audit(&f.db, "p", "s").expect("audit").totals.have_verified,
        1
    );
}

#[test]
fn third_import_demotes_oldest_revision() {
    let mut f = fixture();
    let game = r#"<game name="g"><description>d</description><rom name="g.bin" size="1" crc="deadbeef"/></game>"#;
    let mk = |version: u32| {
        dat(game).replace(
            "<version>1</version>",
            &format!("<version>{version}</version>"),
        )
    };
    let r1 = import_dat(&f.store, &mut f.db, mk(1).as_bytes(), &opts("p", "s"))
        .expect("import 1")
        .revision_id;
    let r2 = import_dat(&f.store, &mut f.db, mk(2).as_bytes(), &opts("p", "s"))
        .expect("import 2")
        .revision_id;
    let report3 =
        import_dat(&f.store, &mut f.db, mk(3).as_bytes(), &opts("p", "s")).expect("import 3");

    assert_eq!(report3.demoted_revisions, vec![r1]);
    let entries_for = |rev: i64| {
        f.db.cache()
            .query_row(
                "SELECT COUNT(*) FROM entry WHERE revision_id = ?1",
                [rev],
                |row| row.get::<_, i64>(0),
            )
            .expect("count")
    };
    assert_eq!(entries_for(r1), 0);
    assert_eq!(entries_for(r2), 1);
    assert_eq!(entries_for(report3.revision_id), 1);
    let materialized: i64 =
        f.db.cache()
            .query_row(
                "SELECT materialized FROM dat_revision WHERE revision_id = ?1",
                [r1],
                |row| row.get(0),
            )
            .expect("materialized");
    assert_eq!(materialized, 0);
}

#[test]
fn dir2dat_round_trips_semantically() {
    let mut f = fixture();
    let original = r#"<?xml version="1.0"?>
<!DOCTYPE datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd">
<datafile>
  <header>
    <name>RT</name><description>Round trip</description><version>7</version>
    <author>tester</author><homepage>example</homepage>
    <clrmamepro header="nes.xml" forcemerging="split" forcepacking="unzip"/>
  </header>
  <game name="child" id="2" cloneofid="1" cloneof="parent" romof="parent">
    <description>Child</description><year>1993</year><manufacturer>Mfg</manufacturer>
    <release name="child" region="EUR" language="en"/>
    <rom name="c.bin" size="16" crc="00112233" md5="{md5}" sha1="{sha1}" merge="p.bin"/>
    <rom name="bad.bin" size="8" crc="44556677" status="baddump"/>
  </game>
  <game name="parent" id="1">
    <description>Parent &amp; Co</description><year>1992</year>
    <rom name="p.bin" size="16" crc="00112233" sha256="{sha256}" mia="yes"/>
    <disk name="p-disk" sha1="{sha1}"/>
    <sample name="boom"/>
  </game>
</datafile>"#
        .replace("{md5}", &"ab".repeat(16))
        .replace("{sha1}", &"cd".repeat(20))
        .replace("{sha256}", &"ef".repeat(32));

    import_dat(&f.store, &mut f.db, original.as_bytes(), &opts("p", "s")).expect("import");
    let exported = export_dat(&f.db, "p", "s", None).expect("export");
    let reparsed = parse(&exported).expect("reparse exported dat");
    let source = parse(original.as_bytes()).expect("parse original");

    // Header fields we preserve.
    for (a, b) in [
        (&source.header.name, &reparsed.header.name),
        (&source.header.description, &reparsed.header.description),
        (&source.header.version, &reparsed.header.version),
        (&source.header.author, &reparsed.header.author),
        (&source.header.homepage, &reparsed.header.homepage),
        (&source.header.force_merging, &reparsed.header.force_merging),
        (&source.header.force_packing, &reparsed.header.force_packing),
        (&source.header.detector, &reparsed.header.detector),
    ] {
        assert_eq!(a, b);
    }

    // Entries: export sorts by name; compare against sorted source.
    let mut src_entries = source.entries.clone();
    src_entries.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(src_entries.len(), reparsed.entries.len());
    for (s, r) in src_entries.iter().zip(&reparsed.entries) {
        assert_eq!(s.name, r.name);
        assert_eq!(s.description, r.description);
        assert_eq!(s.year, r.year);
        assert_eq!(s.manufacturer, r.manufacturer);
        assert_eq!(s.cloneof, r.cloneof);
        assert_eq!(s.romof, r.romof);
        assert_eq!(s.id, r.id);
        assert_eq!(s.cloneof_id, r.cloneof_id);
        assert_eq!(s.releases, r.releases);
        assert_eq!(s.claims.len(), r.claims.len());
        for (sc, rc) in s.claims.iter().zip(&r.claims) {
            assert_eq!(sc.kind, rc.kind);
            assert_eq!(sc.name, rc.name);
            assert_eq!(sc.size, rc.size);
            assert_eq!(sc.crc32, rc.crc32);
            assert_eq!(sc.md5, rc.md5);
            assert_eq!(sc.sha1, rc.sha1);
            assert_eq!(sc.sha256, rc.sha256);
            assert_eq!(sc.status, rc.status);
            assert_eq!(sc.mia, rc.mia);
            assert_eq!(sc.merge_name, rc.merge_name);
        }
    }

    // Determinism: identical DB state → byte-identical export.
    let again = export_dat(&f.db, "p", "s", None).expect("export again");
    assert_eq!(exported, again);
}
