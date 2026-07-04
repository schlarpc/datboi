//! Integration tests: schema/pragmas, alias multi-hit, verify state
//! machine, and the D21 grounding semantics (the load-bearing ones).

use datboi_core::alias::AliasHasher;
use datboi_core::hash::Blake3;
use datboi_index::recipes::NewRecipe;
use datboi_index::{
    AliasAlgo, ClaimKind, ClaimStatus, Db, IndexError, Namespace, OpKind, RecipeSource, Residency,
    SeekClass, VerifyState,
};

fn open_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open(dir.path()).expect("open");
    (dir, db)
}

fn blob(db: &Db, seed: &[u8], residency: Residency) -> i64 {
    db.upsert_blob(&Blake3::compute(seed), Some(64), Namespace::Data, residency)
        .expect("upsert")
}

/// A minimal recipe row: `inputs -> outputs`, already at the given verify
/// state (walking the legal transition chain to get there).
fn recipe(db: &mut Db, seed: &[u8], inputs: &[i64], outputs: &[i64], state: VerifyState) -> i64 {
    let recipe_blob = db
        .upsert_blob(
            &Blake3::compute(seed),
            Some(128),
            Namespace::Meta,
            Residency::Resident,
        )
        .expect("recipe blob");
    let ins: Vec<(u32, i64, Option<&str>)> = inputs
        .iter()
        .enumerate()
        .map(|(i, &b)| (u32::try_from(i).unwrap(), b, None))
        .collect();
    let outs: Vec<(u32, i64, u64, Option<&str>)> = outputs
        .iter()
        .enumerate()
        .map(|(i, &b)| (u32::try_from(i).unwrap(), b, 64, None))
        .collect();
    let recipe_id = db
        .insert_recipe(&NewRecipe {
            blob_id: recipe_blob,
            op_kind: OpKind::Builtin,
            op_name: "assemble@1",
            seek_class: SeekClass::Affine,
            source: RecipeSource::LocalIngest,
            inputs: &ins,
            outputs: &outs,
        })
        .expect("insert recipe");
    if matches!(state, VerifyState::Verified | VerifyState::ReplayedLocal) {
        db.set_verify_state(recipe_id, VerifyState::Verified, 1, None)
            .expect("to verified");
    }
    if state == VerifyState::ReplayedLocal {
        db.set_verify_state(recipe_id, VerifyState::ReplayedLocal, 2, None)
            .expect("to replayed");
    }
    recipe_id
}

#[test]
fn schema_and_pragmas() {
    let (dir, db) = open_db();
    for (conn, synchronous, app_id) in [
        (db.cache(), 1_i64, 0x6474_6263_u32),
        (db.state(), 2_i64, 0x6474_6273_u32),
    ] {
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let sync: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, synchronous);
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        let page: i64 = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(page, 8192);
        let app: u32 = conn
            .query_row("PRAGMA application_id", [], |r| r.get(0))
            .unwrap();
        assert_eq!(app, app_id);
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }
    // Reopen is idempotent (existing files pass validation).
    drop(db);
    Db::open(dir.path()).expect("reopen");
}

#[test]
fn blob_and_alias_round_trip_multi_hit() {
    let (_dir, db) = open_db();
    let hash_a = Blake3::compute(b"blob a");
    let id_a = db
        .upsert_blob(&hash_a, Some(6), Namespace::Data, Residency::Resident)
        .unwrap();
    assert_eq!(db.get_blob_id(&hash_a).unwrap(), Some(id_a));
    // Upsert returns the same id and refreshes residency.
    let again = db
        .upsert_blob(&hash_a, None, Namespace::Data, Residency::Resident)
        .unwrap();
    assert_eq!(again, id_a);

    let mut hasher = AliasHasher::new();
    hasher.update(b"blob a");
    let tuple = hasher.finalize();
    db.insert_aliases(id_a, &tuple).unwrap();
    db.insert_aliases(id_a, &tuple).unwrap(); // idempotent

    assert_eq!(
        db.alias_lookup(AliasAlgo::Sha1, &tuple.sha1).unwrap(),
        vec![id_a]
    );

    // Multi-hit: a second blob claiming the same sha1 digest (D2 collision
    // posture — the alias table must tolerate it).
    let id_b = blob(&db, b"blob b", Residency::Resident);
    let conn = db.cache();
    conn.execute(
        "INSERT INTO alias (algo, digest, blob_id) VALUES (3, ?1, ?2)",
        rusqlite::params![tuple.sha1.as_slice(), id_b],
    )
    .unwrap();
    let mut hits = db.alias_lookup(AliasAlgo::Sha1, &tuple.sha1).unwrap();
    hits.sort_unstable();
    assert_eq!(hits, vec![id_a, id_b]);
}

#[test]
fn recipes_for_output_and_verify_machine() {
    let (_dir, mut db) = open_db();
    let input = blob(&db, b"in", Residency::Resident);
    let output = blob(&db, b"out", Residency::Absent);
    let recipe_id = recipe(&mut db, b"r1", &[input], &[output], VerifyState::Pending);

    let rows = db.recipes_for_output(output).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].recipe_id, recipe_id);
    assert_eq!(rows[0].op_name, "assemble@1");
    assert_eq!(rows[0].verify, VerifyState::Pending);
    assert!(db.recipes_for_output(input).unwrap().is_empty());

    // Legal: pending → verified → replayed-local.
    db.set_verify_state(recipe_id, VerifyState::Verified, 10, None)
        .unwrap();
    // Illegal: downgrade to pending.
    let err = db
        .set_verify_state(recipe_id, VerifyState::Pending, 11, None)
        .unwrap_err();
    assert!(matches!(err, IndexError::IllegalTransition { .. }));
    db.set_verify_state(recipe_id, VerifyState::ReplayedLocal, 12, None)
        .unwrap();

    // Late nondeterminism: replayed-local → failed is legal and terminal.
    db.set_verify_state(
        recipe_id,
        VerifyState::Failed,
        13,
        Some(("hash mismatch on scrub", None)),
    )
    .unwrap();
    for next in [
        VerifyState::Pending,
        VerifyState::Verified,
        VerifyState::ReplayedLocal,
        VerifyState::Failed,
    ] {
        let failure = (next == VerifyState::Failed).then_some(("again", None));
        let err = db
            .set_verify_state(recipe_id, next, 14, failure)
            .unwrap_err();
        assert!(
            matches!(err, IndexError::IllegalTransition { .. }),
            "poison must be terminal, got past {next:?}"
        );
    }
}

#[test]
fn grounding_diamond_and_ungrounded_cycle() {
    let (_dir, mut db) = open_db();
    // Diamond: literals a, b ground c = f(a), d = f(a,b), e = f(c,d).
    let a = blob(&db, b"a", Residency::Resident);
    let b = blob(&db, b"b", Residency::Resident);
    let c = blob(&db, b"c", Residency::Absent);
    let d = blob(&db, b"d", Residency::Absent);
    let e = blob(&db, b"e", Residency::Absent);
    recipe(&mut db, b"rc", &[a], &[c], VerifyState::ReplayedLocal);
    recipe(&mut db, b"rd", &[a, b], &[d], VerifyState::ReplayedLocal);
    recipe(&mut db, b"re", &[c, d], &[e], VerifyState::ReplayedLocal);
    // Merely-verified recipes do NOT ground (D25: replay licenses drops).
    let f = blob(&db, b"f", Residency::Absent);
    recipe(&mut db, b"rf", &[a], &[f], VerifyState::Verified);

    let grounded = db.grounded_set().unwrap();
    for (id, expect) in [(a, true), (b, true), (c, true), (d, true), (e, true)] {
        assert_eq!(grounded.contains(&id), expect, "blob {id}");
    }
    assert!(!grounded.contains(&f), "verified-only must not ground");

    // Malicious claim cycle with no resident ground stays ungrounded (D21).
    let x = blob(&db, b"x", Residency::Absent);
    let y = blob(&db, b"y", Residency::Absent);
    recipe(&mut db, b"rxy", &[x], &[y], VerifyState::ReplayedLocal);
    recipe(&mut db, b"ryx", &[y], &[x], VerifyState::ReplayedLocal);
    let grounded = db.grounded_set().unwrap();
    assert!(!grounded.contains(&x) && !grounded.contains(&y));
}

#[test]
fn evictability_of_mutually_inverse_pair() {
    let (_dir, mut db) = open_db();
    // X ↔ Y (headered/headerless shape): each evictable ALONE — the
    // single-removal semantics that stop circular coverage from dropping
    // both literals (D21).
    let x = blob(&db, b"x", Residency::Resident);
    let y = blob(&db, b"y", Residency::Resident);
    recipe(&mut db, b"x_from_y", &[y], &[x], VerifyState::ReplayedLocal);
    recipe(&mut db, b"y_from_x", &[x], &[y], VerifyState::ReplayedLocal);

    assert!(db.is_evictable(x).unwrap());
    assert!(db.is_evictable(y).unwrap());

    // Once X's literal is actually gone, Y is no longer evictable.
    db.upsert_blob(
        &Blake3::compute(b"x"),
        Some(64),
        Namespace::Data,
        Residency::EvictedCovered,
    )
    .unwrap();
    assert!(!db.is_evictable(y).unwrap());
    // And X itself stays grounded (via Y) — the drop was legal.
    assert!(db.grounded_set().unwrap().contains(&x));
}

#[test]
fn bulk_insert_10k_entries() {
    use datboi_index::dats::{NewClaim, NewEntry};

    let (_dir, mut db) = open_db();
    let source = db.upsert_dat_source("no-intro", "Test System").unwrap();
    assert_eq!(
        db.upsert_dat_source("no-intro", "Test System").unwrap(),
        source
    );
    let dat_blob = blob(&db, b"the dat file", Residency::Resident);
    let revision = db
        .insert_dat_revision(
            source,
            dat_blob,
            0,
            Some("1.0"),
            None,
            Some(r#"{"homepage":"x"}"#),
            None,
            1234,
        )
        .unwrap();
    db.set_current_revision(source, revision).unwrap();

    let names: Vec<String> = (0..10_000).map(|i| format!("Game {i:05}")).collect();
    let entries: Vec<NewEntry<'_>> = names
        .iter()
        .enumerate()
        .map(|(i, name)| NewEntry {
            name,
            stable_key: None,
            description: Some(name),
            year: None,
            manufacturer: None,
            is_bios: false,
            is_device: false,
            is_mechanical: false,
            runnable: true,
            // Every entry past the first is a clone of Game 00000.
            cloneof: (i > 0).then_some("Game 00000"),
            romof: None,
            sampleof: None,
            attrs: (i % 2 == 0).then_some(r#"{"sourcefile":"test.cpp"}"#),
            releases: Vec::new(),
            claims: vec![NewClaim {
                kind: ClaimKind::Rom,
                name: "rom.bin",
                size: Some(4096),
                crc32: Some([1, 2, 3, 4]),
                md5: None,
                sha1: Some([0xab; 20]),
                sha256: None,
                status: ClaimStatus::Good,
                mia: false,
                optional: false,
                merge_name: None,
                attrs: None,
            }],
        })
        .collect();
    let inserted = db.insert_entries(revision, &entries).unwrap();
    assert_eq!(inserted, 10_000);

    let (entry_count, claim_count, resolved): (i64, i64, i64) = db
        .cache()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM entry),
                    (SELECT COUNT(*) FROM rom_claim),
                    (SELECT COUNT(*) FROM entry WHERE cloneof_id IS NOT NULL)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(entry_count, 10_000);
    assert_eq!(claim_count, 10_000);
    assert_eq!(resolved, 9_999, "cloneof names resolve within revision");
}

#[test]
fn state_db_tags_config_snapshot() {
    let (_dir, mut db) = open_db();
    let h = Blake3::compute(b"snapshot root");
    db.set_tag("keep/gba", &h, 1).unwrap();
    assert_eq!(db.get_tag("keep/gba").unwrap(), Some(h));
    db.set_tag("keep/gba", &Blake3::compute(b"moved"), 2)
        .unwrap();
    assert_ne!(db.get_tag("keep/gba").unwrap(), Some(h));
    assert_eq!(db.list_tags().unwrap().len(), 1);
    assert!(db.delete_tag("keep/gba").unwrap());
    assert!(!db.delete_tag("keep/gba").unwrap());

    db.config_set("ingest.default", b"copy").unwrap();
    assert_eq!(
        db.config_get("ingest.default").unwrap().as_deref(),
        Some(b"copy".as_slice())
    );
    assert_eq!(db.config_get("missing").unwrap(), None);

    let seq1 = db.snapshot_log_append(&h, 10).unwrap();
    let seq2 = db.snapshot_log_append(&h, 11).unwrap();
    assert!(seq2 > seq1);

    // truncate_cache leaves state.db alone.
    db.truncate_cache().unwrap();
    assert_eq!(
        db.config_get("ingest.default").unwrap().as_deref(),
        Some(b"copy".as_slice())
    );
    let blobs: i64 = db
        .cache()
        .query_row("SELECT COUNT(*) FROM blob", [], |r| r.get(0))
        .unwrap();
    assert_eq!(blobs, 0);
}
