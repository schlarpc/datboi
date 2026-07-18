//! Integration tests: schema/pragmas, alias multi-hit, verify state
//! machine, and the D21 grounding semantics (the load-bearing ones).

use datboi_core::alias::AliasHasher;
use datboi_core::hash::Blake3;
use datboi_index::recipes::NewRecipe;
use datboi_index::{
    AliasAlgo, ClaimKind, ClaimStatus, Db, IndexError, Namespace, OpKind, RecipeSource, Residency,
    SeekClass, VerifyAdvance, VerifyState,
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
        db.set_verify_state(recipe_id, VerifyAdvance::Verified, 1)
            .expect("to verified");
    }
    if state == VerifyState::ReplayedLocal {
        db.set_verify_state(recipe_id, VerifyAdvance::ReplayedLocal, 2)
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

/// The planner-stats upkeep the serve daemon runs in the background:
/// the at-open form must seed sqlite_stat1 (ANALYZE of never-analyzed
/// tables), and the periodic form must be callable repeatedly.
#[test]
fn optimize_seeds_planner_stats() {
    let (_dir, db) = open_db();
    db.upsert_blob(
        &Blake3::compute(b"stats"),
        Some(1),
        Namespace::Data,
        Residency::Resident,
    )
    .expect("row");
    db.optimize_at_open().expect("optimize at open");
    let stats: i64 = db
        .cache()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'sqlite_stat1'",
            [],
            |r| r.get(0),
        )
        .expect("q");
    assert_eq!(stats, 1, "0x10002 form ANALYZEs unanalyzed tables");
    db.optimize().expect("periodic optimize");
    db.optimize().expect("periodic optimize is repeatable");
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

    // A v1 cache (the shipped ladder's floor): drop the v2+ tables and
    // the v5 index, rewind the stamp — exactly what a real v1 file
    // looks like.
    {
        let conn = rusqlite::Connection::open(dir.path().join("cache.db")).expect("raw open");
        conn.execute_batch(
            "DROP TABLE sweep_queue; DROP TABLE analysis; DROP TABLE seek_quarantine;
             DROP TABLE gc_guard; DROP TABLE orphan_candidate;
             DROP TABLE sweep_absent_eligible; DROP INDEX sf_by_blob;",
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

    // Devolve a copy to v1 (drop everything the ladder creates on a v1
    // table set), then let open() migrate it back up.
    let migrated_dir = tempfile::tempdir().expect("tempdir");
    drop(Db::open(migrated_dir.path()).expect("open"));
    {
        let conn =
            rusqlite::Connection::open(migrated_dir.path().join("cache.db")).expect("raw open");
        conn.execute_batch(
            "DROP TABLE sweep_queue; DROP TABLE analysis; DROP TABLE seek_quarantine;
             DROP TABLE gc_guard; DROP TABLE orphan_candidate;
             DROP TABLE sweep_absent_eligible; DROP INDEX sf_by_blob;",
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

/// Truncation-list completeness: `CACHE_TABLES_CHILD_FIRST` plus the
/// documented exclusions must equal sqlite_master exactly — a new
/// FK-less table forgotten from the list would silently survive
/// "truncation" into a rebuilt cache. The order is also checked
/// against the real FK graph: every child precedes its parents.
#[test]
fn cache_truncation_list_covers_every_table_child_first() {
    use std::collections::HashSet;

    // gc_guard is the one deliberate exclusion (schema.rs): its single
    // seeded row must exist for claims to UPDATE.
    const EXCLUSIONS: &[&str] = &["gc_guard"];

    let (_dir, db) = open_db();
    let actual: HashSet<String> = db
        .cache()
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .expect("q")
        .query_map([], |r| r.get(0))
        .expect("q")
        .collect::<Result<_, _>>()
        .expect("q");
    let listed: HashSet<String> = datboi_index::schema::CACHE_TABLES_CHILD_FIRST
        .iter()
        .chain(EXCLUSIONS)
        .map(|t| (*t).to_owned())
        .collect();
    assert_eq!(
        listed, actual,
        "CACHE_TABLES_CHILD_FIRST ∪ exclusions must equal sqlite_master"
    );

    // Child-first means every FK target sits AFTER its referencing
    // table in the truncation order.
    let position: std::collections::HashMap<&str, usize> =
        datboi_index::schema::CACHE_TABLES_CHILD_FIRST
            .iter()
            .enumerate()
            .map(|(i, t)| (*t, i))
            .collect();
    for (table, &pos) in &position {
        let parents: Vec<String> = db
            .cache()
            .prepare(&format!("PRAGMA foreign_key_list({table})"))
            .expect("q")
            .query_map([], |r| r.get::<_, String>(2))
            .expect("q")
            .collect::<Result<_, _>>()
            .expect("q");
        for parent in parents {
            if parent == *table {
                continue; // self-references truncate fine
            }
            let parent_pos = position
                .get(parent.as_str())
                .unwrap_or_else(|| panic!("{table} references unlisted table {parent}"));
            assert!(
                *parent_pos > pos,
                "{table} (#{pos}) must truncate before its parent {parent} (#{parent_pos})"
            );
        }
    }
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

    // Legal: pending → verified → replayed-local. (A downgrade to
    // Pending is no longer expressible: VerifyAdvance has no such
    // target.)
    db.set_verify_state(recipe_id, VerifyAdvance::Verified, 10)
        .unwrap();
    db.set_verify_state(recipe_id, VerifyAdvance::ReplayedLocal, 12)
        .unwrap();

    // Late nondeterminism: replayed-local → failed is legal and terminal.
    db.set_verify_state(
        recipe_id,
        VerifyAdvance::Failed {
            error: "hash mismatch on scrub",
            peer: None,
        },
        13,
    )
    .unwrap();
    for next in [
        VerifyAdvance::Verified,
        VerifyAdvance::ReplayedLocal,
        VerifyAdvance::Failed {
            error: "again",
            peer: None,
        },
    ] {
        let err = db.set_verify_state(recipe_id, next, 14).unwrap_err();
        assert!(
            matches!(err, IndexError::IllegalTransition { .. }),
            "poison must be terminal, got past {next:?}"
        );
    }
}

/// `index_recipe` derives every row FROM the recipe object: positions,
/// roles, sizes, names, and op kind all come back exactly as the object
/// states them, and a hash the index has never seen gets an Absent blob
/// row — no caller-supplied tuple can drift from the CAS object.
#[test]
fn index_recipe_rows_derive_from_the_object() {
    use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe};

    let (_dir, mut db) = open_db();
    let known_in = Blake3::compute(b"known input");
    let known_id = db
        .upsert_blob(&known_in, Some(64), Namespace::Data, Residency::Resident)
        .unwrap();
    let unknown_in = Blake3::compute(b"input the index never saw");
    let out = Blake3::compute(b"claimed output");
    let recipe = Recipe {
        op: Op::Builtin {
            name: "assemble".into(),
            major: 1,
        },
        inputs: vec![
            InputRef {
                hash: known_in,
                role: Some("skeleton".into()),
            },
            InputRef {
                hash: unknown_in,
                role: None,
            },
        ],
        outputs: vec![OutputRef {
            hash: out,
            size: 524_304,
            name: Some("Game.nes".into()),
        }],
        params: vec![0x80],
    };
    let recipe_blob = blob(&db, b"the recipe object", Residency::Resident);
    assert_eq!(db.recipe_id_for_blob(recipe_blob).unwrap(), None);
    let recipe_id = db
        .index_recipe(
            recipe_blob,
            &recipe,
            SeekClass::Affine,
            RecipeSource::LocalIngest,
        )
        .unwrap();
    assert_eq!(db.recipe_id_for_blob(recipe_blob).unwrap(), Some(recipe_id));

    let row = db.recipe_by_id(recipe_id).unwrap();
    assert_eq!(
        (row.op_kind, row.op_name.as_str(), row.seek_class),
        (OpKind::Builtin, "assemble@1", SeekClass::Affine),
        "op_name is the object's one spelling (Op::index_name)"
    );
    let inputs = db.recipe_inputs(recipe_id).unwrap();
    assert_eq!(inputs.len(), 2);
    assert_eq!(
        (
            inputs[0].position,
            inputs[0].hash,
            inputs[0].role.as_deref()
        ),
        (0, known_in, Some("skeleton"))
    );
    assert_eq!(inputs[0].blob_id, known_id, "existing rows are reused");
    assert_eq!(
        (
            inputs[1].position,
            inputs[1].hash,
            inputs[1].role.as_deref(),
            inputs[1].residency
        ),
        (1, unknown_in, None, Residency::Absent),
        "unseen hashes get Absent rows"
    );
    let (ordinal, size, name): (i64, i64, Option<String>) = db
        .cache()
        .query_row(
            "SELECT ro.ordinal, ro.size, ro.name FROM recipe_output ro
             JOIN blob b ON b.blob_id = ro.blob_id
             WHERE ro.recipe_id = ?1 AND b.hash = ?2",
            rusqlite::params![recipe_id, out.0.as_slice()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (ordinal, size, name.as_deref()),
        (0, 524_304, Some("Game.nes"))
    );
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

/// Job ledger rows decode fallibly like every other coded column: a
/// kind or state code this build doesn't know is an
/// [`IndexError::Decode`], never a silently misfiled row.
#[test]
fn job_ledger_round_trips_and_refuses_unknown_codes() {
    use datboi_index::{JobKind, JobState};

    let (_dir, db) = open_db();
    let id = db
        .insert_finished_job(JobKind::Scrub, "cli: scrub", JobState::Done, 10, 20)
        .unwrap();
    let row = db.job_by_id(id).unwrap().expect("row");
    assert_eq!((row.kind, row.state), (JobKind::Scrub, JobState::Done));

    // A future writer's row: unknown kind code.
    db.state()
        .execute(
            "INSERT INTO job (kind, name, state, started_at) VALUES (99, 'future', 0, 30)",
            [],
        )
        .unwrap();
    let future_id = db.state().last_insert_rowid();
    assert!(matches!(
        db.job_by_id(future_id).unwrap_err(),
        IndexError::Decode {
            what: "JobKind",
            code: 99
        }
    ));
    assert!(matches!(
        db.recent_jobs(10).unwrap_err(),
        IndexError::Decode { .. }
    ));
    assert!(matches!(
        JobState::from_code(99).unwrap_err(),
        IndexError::Decode { .. }
    ));
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

/// `Db::open` owns its preconditions: a missing (even nested) db dir is
/// created rather than surfacing as SQLITE_CANTOPEN — the `serve` --db-dir
/// regression.
#[test]
fn open_creates_missing_db_dir() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = root.path().join("does/not/exist/yet");
    let db = Db::open(&dir).expect("open creates the dir");
    drop(db);
    assert!(dir.join("state.db").is_file());
    assert!(dir.join("cache.db").is_file());
}

/// D71 sweep leases + priority tiers: a claimed item is invisible to
/// other workers until its lease expires or is released; fresh-tier
/// enqueueing outranks dat-matched, never requeues settled analysis,
/// and never gets demoted by the dat bump.
#[test]
fn sweep_leases_and_priority_tiers() {
    use datboi_index::{AnalysisOutcome, PRIORITY_DAT_MATCHED, PRIORITY_FRESH};

    let (_dir, mut db) = open_db();
    let analyzer = Blake3::compute(b"analyzer-x");
    let ambient = blob(&db, b"ambient", Residency::Resident);
    let fresh = blob(&db, b"fresh", Residency::Resident);
    let settled = blob(&db, b"settled", Residency::Resident);
    let _absent = blob(&db, b"absent", Residency::Absent);

    // Settled analysis: enqueue_fresh must not resurrect it.
    db.record_analysis(settled, &analyzer, AnalysisOutcome::Negative, None, 1)
        .unwrap();

    assert_eq!(db.enqueue_unanalyzed(&analyzer, 10).unwrap(), 3);
    // Fresh tier: promotes the queued row, skips the settled blob.
    assert_eq!(
        db.enqueue_fresh(&analyzer, &[fresh, settled], 11).unwrap(),
        1
    );

    // Claim: fresh outranks ambient; the absent blob is never picked.
    let claimed = db.claim_sweep_items(&analyzer, &[], 10, 100, 60).unwrap();
    assert_eq!(
        claimed.iter().map(|i| i.blob_id).collect::<Vec<_>>(),
        [fresh, ambient],
        "fresh first, absent skipped"
    );
    assert_eq!(claimed[0].priority, PRIORITY_FRESH);

    // Leased items are invisible to a second claimant...
    assert!(
        db.claim_sweep_items(&analyzer, &[], 10, 100, 60)
            .unwrap()
            .is_empty()
    );
    // ...visible again after expiry...
    assert_eq!(
        db.claim_sweep_items(&analyzer, &[], 10, 161, 60)
            .unwrap()
            .len(),
        2
    );
    // ...and an early release returns one item without waiting.
    db.release_sweep_lease(ambient, &analyzer).unwrap();
    let reclaimed = db.claim_sweep_items(&analyzer, &[], 10, 162, 60).unwrap();
    assert_eq!(
        reclaimed.iter().map(|i| i.blob_id).collect::<Vec<_>>(),
        [ambient]
    );

    // Startup amnesty: every lease clears at once.
    assert_eq!(db.clear_sweep_leases().unwrap(), 2);
    assert_eq!(
        db.claim_sweep_items(&analyzer, &[], 10, 163, 60)
            .unwrap()
            .len(),
        2
    );

    // Completing removes the row entirely (lease and all).
    db.complete_sweep_item(fresh, &analyzer, AnalysisOutcome::Positive, None, 200)
        .unwrap();
    assert_eq!(db.sweep_queue_len(&analyzer).unwrap(), 2);

    // The dat bump never demotes: re-promote ambient to fresh, bump,
    // and the fresh tier survives.
    db.enqueue_fresh(&analyzer, &[ambient], 201).unwrap();
    db.bump_dat_matched_priorities().unwrap();
    db.clear_sweep_leases().unwrap();
    let after = db.claim_sweep_items(&analyzer, &[], 1, 300, 60).unwrap();
    assert_eq!(after[0].blob_id, ambient);
    assert!(after[0].priority >= PRIORITY_DAT_MATCHED);
    assert_eq!(after[0].priority, PRIORITY_FRESH);
}

/// D108 class gate: a blob is unclaimable for a gated analyzer while a
/// blocking analyzer still holds a queue row for it — leased or not (a
/// row means unsettled) — and opens the moment the blocker concludes.
/// Blobs the blocker never queued flow through untouched.
#[test]
fn sweep_claim_class_gate() {
    use datboi_index::AnalysisOutcome;

    let (_dir, mut db) = open_db();
    let structural = Blake3::compute(b"structural-analyzer");
    let fallback = Blake3::compute(b"fallback-analyzer");
    let gated = blob(&db, b"gated", Residency::Resident);
    let free = blob(&db, b"free", Residency::Resident);

    // The fresh path enqueues blocker families first (roster order,
    // D108): only `gated` is on the structural queue.
    db.enqueue_fresh(&structural, &[gated], 10).unwrap();
    db.enqueue_fresh(&fallback, &[gated, free], 10).unwrap();

    let claimed = db
        .claim_sweep_items(&fallback, &[structural], 10, 100, 60)
        .unwrap();
    assert_eq!(
        claimed.iter().map(|i| i.blob_id).collect::<Vec<_>>(),
        [free],
        "the structural row gates the shared blob; the free blob flows"
    );
    db.complete_sweep_item(free, &fallback, AnalysisOutcome::Negative, None, 101)
        .unwrap();

    // A LEASED blocker row still gates: in flight is not settled.
    let in_flight = db.claim_sweep_items(&structural, &[], 10, 110, 60).unwrap();
    assert_eq!(in_flight.len(), 1);
    assert!(
        db.claim_sweep_items(&fallback, &[structural], 10, 111, 60)
            .unwrap()
            .is_empty()
    );

    // The blocker concluding removes its row and opens the gate.
    db.complete_sweep_item(gated, &structural, AnalysisOutcome::Positive, None, 120)
        .unwrap();
    let opened = db
        .claim_sweep_items(&fallback, &[structural], 10, 200, 60)
        .unwrap();
    assert_eq!(
        opened.iter().map(|i| i.blob_id).collect::<Vec<_>>(),
        [gated]
    );
}

/// D71 progress-gated heartbeat: renewal through a `SweepLeaseKeeper`
/// (its own connection) extends a lease past its original expiry, and
/// the item frees on the RENEWED clock once progress stops.
#[test]
fn sweep_lease_renewal_extends_visibility() {
    let (_dir, mut db) = open_db();
    let analyzer = Blake3::compute(b"analyzer-y");
    let target = blob(&db, b"long-runner", Residency::Resident);
    db.enqueue_unanalyzed(&analyzer, 10).unwrap();
    let claimed = db.claim_sweep_items(&analyzer, &[], 1, 100, 60).unwrap();
    assert_eq!(claimed[0].blob_id, target);

    // Progress at t=150 re-stamps: expiry moves from 160 to 210.
    let keeper = db.lease_keeper().unwrap();
    keeper.renew(target, &analyzer, 150, 60).unwrap();
    assert!(
        db.claim_sweep_items(&analyzer, &[], 1, 161, 60)
            .unwrap()
            .is_empty(),
        "past the ORIGINAL expiry the renewed lease still holds"
    );
    assert_eq!(
        db.claim_sweep_items(&analyzer, &[], 1, 211, 60)
            .unwrap()
            .len(),
        1,
        "no further renewals: the item frees on the renewed clock"
    );

    // Renewing a completed (deleted) item is a harmless no-op.
    db.complete_sweep_item(
        target,
        &analyzer,
        datboi_index::AnalysisOutcome::Negative,
        None,
        300,
    )
    .unwrap();
    keeper.renew(target, &analyzer, 301, 60).unwrap();
    assert_eq!(db.sweep_queue_len(&analyzer).unwrap(), 0);
}

/// D72 singleton guard: one winner at a time; expiry lets a successor
/// steal; release is holder-checked; claim doubles as renewal.
#[test]
fn gc_guard_single_holder_with_expiry() {
    use datboi_index::GuardHolder;
    let (_dir, db) = open_db();
    let a = GuardHolder([1; 16]);
    let b = GuardHolder([2; 16]);

    assert!(
        db.claim_gc_guard(&a, 100, 60).unwrap(),
        "free guard: A wins"
    );
    assert!(!db.claim_gc_guard(&b, 120, 60).unwrap(), "held: B loses");
    assert!(
        db.claim_gc_guard(&a, 130, 60).unwrap(),
        "A re-claims = renews"
    );
    assert!(
        !db.claim_gc_guard(&b, 189, 60).unwrap(),
        "renewed lease holds"
    );
    assert!(db.claim_gc_guard(&b, 191, 60).unwrap(), "expired: B steals");
    // A's release must not free B's guard.
    db.release_gc_guard(&a).unwrap();
    assert!(!db.claim_gc_guard(&a, 200, 60).unwrap(), "B still holds");
    db.release_gc_guard(&b).unwrap();
    assert!(
        db.claim_gc_guard(&a, 210, 60).unwrap(),
        "released: free again"
    );
}

/// D73 orphan lifecycle: mark preserves the first-seen clock, anything
/// that roots a blob clears its mark, grace gates review, delete-time
/// re-verification refuses rooted blobs, and row deletion cascades.
#[test]
fn orphan_marks_clear_on_rooting_and_delete_reverifies() {
    let (_dir, mut db) = open_db();
    let junk = blob(&db, b"junk upload", Residency::Resident);
    let wanted = blob(&db, b"becomes wanted", Residency::Resident);
    let _queued = blob(&db, b"awaiting analysis", Residency::Resident);
    db.enqueue_unanalyzed(&Blake3::compute(b"an"), 5).unwrap();
    // Only `queued` stays in a sweep queue (complete the others).
    for b in [junk, wanted] {
        db.complete_sweep_item(
            b,
            &Blake3::compute(b"an"),
            datboi_index::AnalysisOutcome::Negative,
            None,
            6,
        )
        .unwrap();
    }

    let (marked, cleared) = db.sweep_orphan_marks(&[], 10).unwrap();
    assert_eq!(
        (marked, cleared),
        (2, 0),
        "junk + wanted marked; queued spared"
    );

    // A recipe roots `wanted`: the next sweep clears its mark and the
    // first-seen clock of `junk` survives re-sweeps.
    recipe(&mut db, b"r", &[wanted], &[junk], VerifyState::Pending);
    let (marked, cleared) = db.sweep_orphan_marks(&[], 50).unwrap();
    assert_eq!(
        (marked, cleared),
        (0, 2),
        "wanted rooted; junk now recipe-output-rooted too"
    );

    // A genuinely junk blob ages through grace into the review set.
    let lone = blob(&db, b"lone junk", Residency::Resident);
    db.sweep_orphan_marks(&[], 100).unwrap();
    assert!(
        db.list_orphan_candidates(110, 60).unwrap().is_empty(),
        "grace not elapsed"
    );
    let listed = db.list_orphan_candidates(170, 60).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].blob_id, lone);

    // Delete-time re-verification: extra roots refuse, aging holds.
    assert!(db.orphan_still_deletable(lone, &[], 170, 60).unwrap());
    assert!(!db.orphan_still_deletable(lone, &[lone], 170, 60).unwrap());
    assert!(
        !db.orphan_still_deletable(lone, &[], 130, 60).unwrap(),
        "not aged"
    );

    // Row deletion cascades and the blob is gone.
    db.delete_orphan_rows(lone).unwrap();
    assert_eq!(
        db.get_blob_id(&Blake3::compute(b"lone junk")).unwrap(),
        None
    );
    assert!(db.list_orphan_candidates(200, 0).unwrap().is_empty());
}

/// D92: the claim gate admits non-resident items exactly when the
/// admission table says their bytes are obtainable — grounded through
/// trusted claims, within the molten eagerness policy. Ungrounded
/// rumors never claim; the dat-named default admits named members and
/// (unconditionally) evicted-covered blobs; `all` widens to every
/// grounded absent; `off` restores the pre-D92 posture.
#[test]
fn absent_eligibility_gates_the_claim_query() {
    use datboi_index::AbsentMode;

    let (_dir, mut db) = open_db();
    let analyzer = Blake3::compute(b"analyzer-d92");

    let container = blob(&db, b"resident container", Residency::Resident);
    let member = blob(&db, b"claimed member", Residency::Absent);
    let rumor = blob(&db, b"peer rumor", Residency::Absent);
    let evicted = blob(&db, b"evicted blob", Residency::EvictedCovered);
    // A verified local claim grounds the member in the container; a
    // licensed route grounds the evicted blob. The rumor has nothing.
    recipe(
        &mut db,
        b"r-member",
        &[container],
        &[member],
        VerifyState::Verified,
    );
    recipe(
        &mut db,
        b"r-evicted",
        &[container],
        &[evicted],
        VerifyState::ReplayedLocal,
    );
    db.enqueue_unanalyzed(&analyzer, 1).unwrap();

    let claim_ids = |db: &mut Db, at: i64| -> Vec<i64> {
        let ids = db
            .claim_sweep_items(&analyzer, &[], 10, at, 60)
            .unwrap()
            .iter()
            .map(|i| i.blob_id)
            .collect();
        db.clear_sweep_leases().unwrap();
        ids
    };

    // Never refreshed (empty admission table): the resident-only shape.
    assert_eq!(claim_ids(&mut db, 100), [container]);

    // Default (dat-named), nothing dat-named yet: the evicted blob is
    // admitted unconditionally; the member claim and the rumor are not.
    db.refresh_absent_eligibility().unwrap();
    assert_eq!(claim_ids(&mut db, 200), [container, evicted]);

    // A dat names the member: admitted under the default mode.
    db.cache()
        .execute_batch(
            "INSERT INTO dat_source (provider, system) VALUES ('t', 't');
             INSERT INTO dat_revision (source_id, blob_id, format, imported_at)
               VALUES (1, 1, 0, 0);
             INSERT INTO entry (revision_id, name) VALUES (1, 'game');
             INSERT INTO content_identity (size, strength) VALUES (64, 1);
             INSERT INTO rom_claim (entry_id, kind, name, size) VALUES (1, 0, 'game.nds', 64);",
        )
        .unwrap();
    db.cache()
        .execute(
            "INSERT INTO identity_blob (identity_id, blob_id, basis) VALUES (1, ?1, 1)",
            [member],
        )
        .unwrap();
    // rom_claim.identity_id links the claim to the identity.
    db.cache()
        .execute("UPDATE rom_claim SET identity_id = 1", [])
        .unwrap();
    db.refresh_absent_eligibility().unwrap();
    assert_eq!(claim_ids(&mut db, 300), [container, member, evicted]);

    // `all` widens to every grounded absent — which still excludes the
    // ungrounded rumor (eligibility is grounding first, policy second).
    db.set_absent_mode(Some(AbsentMode::All)).unwrap();
    db.refresh_absent_eligibility().unwrap();
    let ids = claim_ids(&mut db, 400);
    assert_eq!(ids, [container, member, evicted]);
    assert!(!ids.contains(&rumor), "ungrounded claims never admitted");

    // `off` restores the pre-D92 posture.
    db.set_absent_mode(Some(AbsentMode::Off)).unwrap();
    assert_eq!(db.refresh_absent_eligibility().unwrap(), 0);
    assert_eq!(claim_ids(&mut db, 500), [container]);
}

/// D93: the read pool's fence is mechanical — a read-only handle
/// serves queries, refuses writes at the sqlite layer (a
/// misclassified handler errors loudly, never corrupts quietly), and
/// still verifies identity + schema version so a stale reader can't
/// misread a newer schema.
#[test]
fn read_only_open_reads_but_never_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hash = Blake3::compute(b"visible to readers");
    {
        let db = Db::open(dir.path()).expect("rw open");
        db.upsert_blob(&hash, Some(1), Namespace::Data, Residency::Resident)
            .expect("row");
        db.config_set("k", b"v").expect("cfg");
    }
    let ro = Db::open_read_only(dir.path()).expect("ro open");
    assert!(ro.get_blob_id(&hash).expect("read works").is_some());
    assert_eq!(
        ro.config_get("k").expect("read works").as_deref(),
        Some(&b"v"[..])
    );
    // Writes refuse on both files.
    assert!(
        ro.upsert_blob(
            &Blake3::compute(b"x"),
            None,
            Namespace::Data,
            Residency::Resident
        )
        .is_err(),
        "cache write must refuse"
    );
    assert!(
        ro.config_set("k2", b"v").is_err(),
        "state write must refuse"
    );

    // A version-skewed file refuses to open read-only (no migrations
    // from a handle that can't write them).
    {
        let conn = rusqlite::Connection::open(dir.path().join("cache.db")).expect("raw");
        conn.pragma_update(None, "user_version", 1).expect("rewind");
    }
    assert!(matches!(
        Db::open_read_only(dir.path()),
        Err(IndexError::SchemaVersion { .. })
    ));
}

/// The D102 roots scope: resident Data blobs with no non-Failed
/// producing route — underived literals only. A Failed route does not
/// disqualify (poison proves nothing about the bytes' origin); ANY
/// live route does, wherever it leads (coverage is the plans scope's
/// job); non-resident and Meta rows never advertise.
#[test]
fn root_blobs_are_the_underived_resident_literals() {
    let (_dir, mut db) = open_db();

    // A loose never-analyzed ROM: resident, no routes → root.
    let loose = blob(&db, b"loose-rom", Residency::Resident);
    // A decomposed container: resident, produced by a live route → not
    // a root, even though its producer's input is a root.
    let piece = blob(&db, b"piece", Residency::Resident);
    let container = blob(&db, b"container", Residency::Resident);
    recipe(
        &mut db,
        b"rebuild",
        &[piece],
        &[container],
        VerifyState::Pending,
    );
    // Only a FAILED route produces this one → still a root.
    let poisoned_out = blob(&db, b"poisoned-out", Residency::Resident);
    let poisoned = recipe(
        &mut db,
        b"lying",
        &[piece],
        &[poisoned_out],
        VerifyState::Pending,
    );
    db.set_verify_state(
        poisoned,
        VerifyAdvance::Failed {
            error: "lied",
            peer: None,
        },
        3,
    )
    .expect("poison");
    // Absent and EvictedCovered rows are not servable literals; Meta
    // blobs (the recipe objects minted above) ride the plans scope.
    blob(&db, b"absent", Residency::Absent);
    blob(&db, b"evicted", Residency::EvictedCovered);

    let mut roots = db.root_blobs().expect("roots");
    roots.sort_unstable_by_key(|h| h.0);
    let mut want = vec![
        Blake3::compute(b"loose-rom"),
        Blake3::compute(b"piece"),
        Blake3::compute(b"poisoned-out"),
    ];
    want.sort_unstable_by_key(|h| h.0);
    assert_eq!(roots, want);
    assert_eq!(db.root_blob_count().expect("count"), 3);

    // The streaming twin visits the identical set (the recon
    // responder's pass).
    let mut streamed = Vec::new();
    db.for_each_root_blob(&mut |h| streamed.push(h))
        .expect("stream");
    streamed.sort_unstable_by_key(|h| h.0);
    assert_eq!(streamed, want);

    // Analysis migrates a blob out of the roots scope: decomposing the
    // loose ROM (minting a producing route) moves it under plan
    // coverage — the D102 invisibility class shrinking by definition.
    let slice = blob(&db, b"slice-of-loose", Residency::Resident);
    recipe(
        &mut db,
        b"loose-rebuild",
        &[slice],
        &[loose],
        VerifyState::Pending,
    );
    assert!(
        !db.root_blobs()
            .expect("roots")
            .contains(&Blake3::compute(b"loose-rom")),
        "a produced blob leaves the roots scope"
    );
}
