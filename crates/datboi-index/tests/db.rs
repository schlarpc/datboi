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
    for (conn, synchronous, app_id, expected_version) in [
        (
            db.cache(),
            1_i64,
            0x6474_6263_u32,
            datboi_index::schema::CACHE_SCHEMA_VERSION,
        ),
        (
            db.state(),
            2_i64,
            0x6474_6273_u32,
            datboi_index::schema::STATE_SCHEMA_VERSION,
        ),
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
        assert_eq!(version, expected_version);
    }
    // Reopen is idempotent (existing files pass validation).
    drop(db);
    Db::open(dir.path()).expect("reopen");
}

/// D37's split, made mechanical: an older cache.db migrates in place
/// when a ladder step exists (rows survive), falls back to
/// drop-and-recreate when it can't; state.db (authoritative) is never
/// dropped; a future-versioned file of either kind refuses to open (no
/// downgrades).
#[test]
fn version_skew_recreates_cache_and_protects_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cached = Blake3::compute(b"cached row");
    {
        let db = Db::open(dir.path()).expect("open");
        db.upsert_blob(&cached, Some(1), Namespace::Data, Residency::Resident)
            .expect("row");
        // Authoritative row that must survive whatever the cache does.
        db.state()
            .execute(
                "INSERT INTO config (key, value) VALUES ('precious', x'01')",
                [],
            )
            .expect("state row");
    }

    // A v1 cache (the shipped ladder's floor): drop the v2 tables and
    // rewind the stamp, exactly what a real v1 file looks like.
    {
        let conn = rusqlite::Connection::open(dir.path().join("cache.db")).expect("raw open");
        conn.execute_batch(
            "DROP TABLE sweep_queue; DROP TABLE analysis; DROP TABLE seek_quarantine;",
        )
        .expect("devolve");
        conn.pragma_update(None, "user_version", 1).expect("rewind");
    }
    {
        let db = Db::open(dir.path()).expect("reopen migrates in place");
        assert_eq!(
            db.get_blob_id(&cached).expect("q"),
            Some(1),
            "in-place migration keeps cache rows"
        );
        let version: u32 = db
            .cache()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("q");
        assert_eq!(version, datboi_index::schema::CACHE_SCHEMA_VERSION);
        // The migrated tables work.
        db.quarantine_seek(&Blake3::compute(b"c"), 1, "test")
            .expect("v2 table live");
    }

    // A version below the ladder's floor (never stamped): the fallback
    // recreates the cache empty; authoritative state is untouched.
    {
        let conn = rusqlite::Connection::open(dir.path().join("cache.db")).expect("raw open");
        conn.pragma_update(None, "user_version", 0).expect("rewind");
    }
    let db = Db::open(dir.path()).expect("reopen recreates");
    let blobs: i64 = db
        .cache()
        .query_row("SELECT COUNT(*) FROM blob", [], |r| r.get(0))
        .expect("q");
    assert_eq!(blobs, 0, "unreachable version fell back to recreate");
    let precious: i64 = db
        .state()
        .query_row(
            "SELECT COUNT(*) FROM config WHERE key = 'precious'",
            [],
            |r| r.get(0),
        )
        .expect("q");
    assert_eq!(precious, 1, "authoritative state untouched");
    drop(db);

    // A FUTURE version (downgrade scenario) refuses for both files.
    for file in ["cache.db", "state.db"] {
        let conn = rusqlite::Connection::open(dir.path().join(file)).expect("raw open");
        conn.pragma_update(None, "user_version", 9999)
            .expect("fast-forward");
        drop(conn);
        let err = match Db::open(dir.path()) {
            Ok(_) => panic!("{file}: future version must refuse to open"),
            Err(e) => e,
        };
        assert!(matches!(err, IndexError::SchemaVersion { .. }), "{file}");
        let conn = rusqlite::Connection::open(dir.path().join(file)).expect("raw open");
        let restore = if file == "cache.db" {
            datboi_index::schema::CACHE_SCHEMA_VERSION
        } else {
            datboi_index::schema::STATE_SCHEMA_VERSION
        };
        conn.pragma_update(None, "user_version", restore)
            .expect("restore");
    }
    Db::open(dir.path()).expect("healthy again");
}

/// The anti-drift guarantee for the cache ladder: devolving a fresh
/// schema to the ladder's floor and migrating back up yields shapes
/// IDENTICAL to a fresh CACHE_DDL. If a future DDL edit forgets its
/// migration step (or the step diverges from the DDL), this fails.
#[test]
fn migrated_cache_equals_fresh_schema() {
    let fresh_dir = tempfile::tempdir().expect("tempdir");
    let fresh = Db::open(fresh_dir.path()).expect("open");
    let shapes = |conn: &rusqlite::Connection| -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT name, sql FROM sqlite_master
                 WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .expect("q");
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("q")
            .collect::<Result<Vec<_>, _>>()
            .expect("q")
    };
    let fresh_shapes = shapes(fresh.cache());

    // Devolve a copy to v1 (drop everything the v1→v2 step creates),
    // then let open() migrate it back up.
    let migrated_dir = tempfile::tempdir().expect("tempdir");
    drop(Db::open(migrated_dir.path()).expect("open"));
    {
        let conn =
            rusqlite::Connection::open(migrated_dir.path().join("cache.db")).expect("raw open");
        conn.execute_batch(
            "DROP TABLE sweep_queue; DROP TABLE analysis; DROP TABLE seek_quarantine;",
        )
        .expect("devolve");
        conn.pragma_update(None, "user_version", 1).expect("rewind");
    }
    let migrated = Db::open(migrated_dir.path()).expect("migrates");
    assert_eq!(
        shapes(migrated.cache()),
        fresh_shapes,
        "CACHE_MIGRATIONS must reproduce CACHE_DDL exactly"
    );
}

/// The anti-drift guarantee for the STATE ladder. ALTER TABLE rewrites
/// the stored CREATE text differently from a fresh CREATE, so this
/// compares normalized shapes (table_info + index list) instead of raw
/// sql — same guarantee: a DDL edit without its ladder step fails here.
#[test]
fn migrated_state_equals_fresh_schema() {
    /// The shipped v1 state DDL, frozen here the way the ladder itself
    /// is frozen: a real v1 file is exactly this.
    const V1_STATE_DDL: &str = r"
CREATE TABLE tag (name TEXT PRIMARY KEY, hash BLOB NOT NULL, created_at INTEGER NOT NULL) STRICT;
CREATE TABLE user (user_id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE,
  argon2 TEXT NOT NULL, role INTEGER NOT NULL, created_at INTEGER NOT NULL) STRICT;
CREATE TABLE invite (token_hash BLOB PRIMARY KEY, created_by INTEGER REFERENCES user(user_id),
  expires_at INTEGER NOT NULL, used_by INTEGER) STRICT;
CREATE TABLE session (token_hash BLOB PRIMARY KEY, user_id INTEGER NOT NULL,
  expires_at INTEGER NOT NULL) STRICT;
CREATE TABLE peer_acl (node_id BLOB PRIMARY KEY, label TEXT, granted INTEGER NOT NULL) STRICT;
CREATE TABLE view_def (name TEXT PRIMARY KEY, definition BLOB NOT NULL,
  updated_at INTEGER NOT NULL) STRICT;
CREATE TABLE channel (name TEXT PRIMARY KEY, kind INTEGER NOT NULL, promotion INTEGER NOT NULL,
  head_hash BLOB, seq INTEGER NOT NULL DEFAULT 0) STRICT;
CREATE TABLE subscription (peer_node BLOB NOT NULL, channel TEXT NOT NULL, policy INTEGER NOT NULL,
  pinned_head BLOB, PRIMARY KEY (peer_node, channel)) STRICT;
CREATE TABLE config (key TEXT PRIMARY KEY, value BLOB NOT NULL) STRICT;
CREATE TABLE snapshot_log (seq INTEGER PRIMARY KEY, hash BLOB NOT NULL,
  created_at INTEGER NOT NULL) STRICT;
";

    // Normalized shapes: for every table, its table_info rows; plus the
    // (auto-)index name list. Formatting-insensitive on purpose.
    let shapes = |conn: &rusqlite::Connection| -> Vec<String> {
        let mut out = Vec::new();
        let mut stmt = conn
            .prepare(
                "SELECT type, name FROM sqlite_master
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .expect("q");
        let objects: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("q")
            .collect::<Result<_, _>>()
            .expect("q");
        for (kind, name) in objects {
            out.push(format!("{kind} {name}"));
            if kind == "table" {
                let mut info = conn
                    .prepare(&format!("PRAGMA table_info({name})"))
                    .expect("q");
                let cols: Vec<String> = info
                    .query_map([], |r| {
                        Ok(format!(
                            "  {} {} notnull={} dflt={:?} pk={}",
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, i64>(5)?,
                        ))
                    })
                    .expect("q")
                    .collect::<Result<_, _>>()
                    .expect("q");
                out.extend(cols);
            }
        }
        out
    };

    let fresh_dir = tempfile::tempdir().expect("tempdir");
    let fresh = Db::open(fresh_dir.path()).expect("open");
    let fresh_shapes = shapes(fresh.state());

    // Build a real v1 state.db from the frozen DDL, then let open()
    // walk the ladder.
    let v1_dir = tempfile::tempdir().expect("tempdir");
    {
        let conn = rusqlite::Connection::open(v1_dir.path().join("state.db")).expect("raw open");
        conn.execute_batch(V1_STATE_DDL).expect("v1 ddl");
        conn.pragma_update(None, "application_id", 0x6474_6273_u32)
            .expect("stamp app");
        conn.pragma_update(None, "user_version", 1)
            .expect("stamp v1");
        // A row that must survive the migration, with the role default
        // backfilling to friend (least privilege).
        conn.execute(
            "INSERT INTO invite (token_hash, expires_at) VALUES (x'11', 9999)",
            [],
        )
        .expect("v1 row");
    }
    let migrated = Db::open(v1_dir.path()).expect("migrates");
    assert_eq!(
        shapes(migrated.state()),
        fresh_shapes,
        "STATE_MIGRATIONS must reproduce STATE_DDL shapes exactly"
    );
    let version: u32 = migrated
        .state()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("q");
    assert_eq!(version, datboi_index::schema::STATE_SCHEMA_VERSION);
    let role: i64 = migrated
        .state()
        .query_row("SELECT role FROM invite", [], |r| r.get(0))
        .expect("row survived");
    assert_eq!(role, datboi_index::Role::Friend.code(), "backfill default");
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

#[test]
fn auth_users_invites_sessions_grants() {
    use datboi_index::{InviteOutcome, Role};

    let (_dir, db) = open_db();

    // -- invites: single-use, expiring, role-carrying (D68) --
    let invite = [0xaa; 32];
    db.mint_invite(&invite, None, Role::Friend, 1_000).unwrap();

    // wrong token / expired token both answer InviteInvalid
    assert_eq!(
        db.accept_invite(&[0xbb; 32], "mika", "$phc$", 500).unwrap(),
        InviteOutcome::InviteInvalid
    );
    assert_eq!(
        db.accept_invite(&invite, "mika", "$phc$", 1_000).unwrap(),
        InviteOutcome::InviteInvalid,
        "expires_at is exclusive"
    );

    // a taken username must NOT consume the invite
    let squatter = db.create_user("mika", "$phc$", Role::Owner, 1).unwrap();
    assert_eq!(
        db.accept_invite(&invite, "mika", "$phc$", 500).unwrap(),
        InviteOutcome::UsernameTaken
    );

    // acceptance creates the user with the INVITE's role...
    let outcome = db.accept_invite(&invite, "pal", "$phc2$", 500).unwrap();
    let InviteOutcome::Accepted { user_id, role } = outcome else {
        panic!("expected acceptance, got {outcome:?}");
    };
    assert_eq!(role, Role::Friend);
    let pal = db.user_by_name("pal").unwrap().expect("created");
    assert_eq!(
        (pal.user_id, pal.role, pal.argon2.as_str()),
        (user_id, Role::Friend, "$phc2$")
    );

    // ...exactly once
    assert_eq!(
        db.accept_invite(&invite, "other", "$phc$", 500).unwrap(),
        InviteOutcome::InviteInvalid,
        "single-use"
    );

    assert_eq!(db.user_by_name("nobody").unwrap().map(|u| u.user_id), None);
    let names: Vec<_> = db
        .list_users()
        .unwrap()
        .into_iter()
        .map(|u| u.username)
        .collect();
    assert_eq!(names, ["mika", "pal"]);

    // -- sessions: expiry-checked lookup, revocation --
    let (s1, s2) = ([0x01; 32], [0x02; 32]);
    db.create_session(&s1, user_id, 2_000).unwrap();
    db.create_session(&s2, user_id, 3_000).unwrap();
    assert_eq!(
        db.session_user(&s1, 1_500).unwrap(),
        Some((user_id, "pal".to_owned(), Role::Friend))
    );
    assert_eq!(db.session_user(&s1, 2_000).unwrap(), None, "expired");
    assert_eq!(db.session_user(&[0xff; 32], 1_500).unwrap(), None);
    assert_eq!(db.list_sessions().unwrap().len(), 2);
    assert_eq!(db.delete_expired_sessions(2_500).unwrap(), 1);
    assert!(db.delete_session(&s2).unwrap());
    assert!(!db.delete_session(&s2).unwrap(), "already gone");
    db.create_session(&s1, user_id, 9_000).unwrap();
    db.create_session(&s2, user_id, 9_000).unwrap();
    assert_eq!(db.delete_sessions_for_user(user_id).unwrap(), 2);
    assert!(db.list_sessions().unwrap().is_empty());

    // -- view grants (the friend-surface ACL) --
    db.grant_view(user_id, "gba").unwrap();
    db.grant_view(user_id, "gba").unwrap(); // idempotent
    db.grant_view(user_id, "psx").unwrap();
    db.grant_view(squatter, "gba").unwrap();
    assert_eq!(db.grants_for_user(user_id).unwrap(), ["gba", "psx"]);
    assert_eq!(db.all_grants().unwrap().len(), 3);
    assert!(db.revoke_view(user_id, "psx").unwrap());
    assert!(!db.revoke_view(user_id, "psx").unwrap(), "already revoked");
    assert_eq!(db.grants_for_user(user_id).unwrap(), ["gba"]);
}
