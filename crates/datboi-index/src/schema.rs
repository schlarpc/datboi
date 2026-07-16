//! DDL for both database files (docs/schema.md §1–§4).
//!
//! cache.db tables are derivable caches (D15): any of them may be dropped
//! and rebuilt from CAS bytes + deterministic re-import. state.db tables
//! are authoritative until snapshotted into CAS (D37). Tables are STRICT;
//! hashes are 32-byte BLOBs; graph tables use integer surrogate keys.

/// The two files version INDEPENDENTLY (D37 made mechanical): a cache
/// schema change must never touch the authoritative file's openability.
///
/// cache.db upgrade policy: in-place [`CACHE_MIGRATIONS`] when a ladder
/// step exists, else drop-and-recreate empty (derivable by definition —
/// `datboi recover` or a rescan repopulates, and D37's "cavalier
/// migrations" license lives exactly in that fallback). At 10M-blob
/// scale a rebuild is a full NFS metadata walk, so routine bumps should
/// always ship a step; the fallback is for changes not worth one.
/// v2: seek_quarantine (D49), analysis + sweep_queue (D45/D48).
/// v3: sweep_queue.leased_until (D71 in-daemon refinement leases).
/// v4: gc_guard + orphan_candidate (D72/D73 background GC).
/// v5: sf_by_blob + analyzer-first sweep_by_priority (query-shaped
/// indexes; no table changes).
/// v6: sweep_absent_eligible (D92 grounded-not-resident sweeps).
pub const CACHE_SCHEMA_VERSION: u32 = 6;

/// cache.db migration ladder, same shape and rules as
/// [`STATE_MIGRATIONS`]: `CACHE_MIGRATIONS[i]` migrates version `i + 1`
/// to `i + 2`; shipped steps are append-only and immutable. Each step
/// must produce shapes identical to a fresh [`CACHE_DDL`] — the
/// `migrated_cache_equals_fresh_schema` test enforces it.
pub const CACHE_MIGRATIONS: &[&str] = &[
    // v1 → v2: seek_quarantine (D49), analysis + sweep_queue (D45/D48) —
    // purely additive.
    "
CREATE TABLE seek_quarantine (
  component      BLOB PRIMARY KEY,
  quarantined_at INTEGER NOT NULL,
  reason         TEXT NOT NULL
) STRICT, WITHOUT ROWID;
CREATE TABLE analysis (
  blob_id     INTEGER NOT NULL REFERENCES blob(blob_id),
  analyzer    BLOB NOT NULL,
  outcome     INTEGER NOT NULL,
  detail      TEXT,
  analyzed_at INTEGER NOT NULL,
  PRIMARY KEY (blob_id, analyzer)
) STRICT, WITHOUT ROWID;
CREATE INDEX analysis_by_analyzer ON analysis(analyzer);
CREATE TABLE sweep_queue (
  blob_id     INTEGER NOT NULL REFERENCES blob(blob_id),
  analyzer    BLOB NOT NULL,
  priority    INTEGER NOT NULL DEFAULT 0,
  enqueued_at INTEGER NOT NULL,
  PRIMARY KEY (blob_id, analyzer)
) STRICT, WITHOUT ROWID;
CREATE INDEX sweep_by_priority ON sweep_queue(priority DESC, enqueued_at);
",
    // v2 → v3 (D71): per-item sweep leases, so the in-daemon refine
    // worker and a concurrent CLI sweep never duplicate an expensive
    // analysis. Recreate rather than ALTER: the queue is scheduling
    // state (enqueue_unanalyzed regrows it on the next sweep), and a
    // fresh CREATE keeps the stored schema text identical to CACHE_DDL
    // (the migrated_cache_equals_fresh_schema guarantee).
    "
DROP TABLE sweep_queue;
CREATE TABLE sweep_queue (
  blob_id      INTEGER NOT NULL REFERENCES blob(blob_id),
  analyzer     BLOB NOT NULL,
  priority     INTEGER NOT NULL DEFAULT 0,
  enqueued_at  INTEGER NOT NULL,
  leased_until INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (blob_id, analyzer)
) STRICT, WITHOUT ROWID;
CREATE INDEX sweep_by_priority ON sweep_queue(priority DESC, enqueued_at);
",
    // v3 → v4 (D72/D73): the eviction singleton guard (the ONE
    // correctness lease — see D72) and orphan candidate marks
    // (cache-grade; a re-sweep regrows them).
    "
CREATE TABLE gc_guard (
  guard_id   INTEGER PRIMARY KEY CHECK (guard_id = 1),
  holder     BLOB,
  expires_at INTEGER NOT NULL DEFAULT 0
) STRICT;
INSERT INTO gc_guard (guard_id, holder, expires_at) VALUES (1, NULL, 0);
CREATE TABLE orphan_candidate (
  blob_id   INTEGER PRIMARY KEY REFERENCES blob(blob_id),
  marked_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
",
    // v4 → v5: query-shaped indexes, no table changes. sf_by_blob
    // serves the blob-detail surfaces (routes, paths, provenance) that
    // look up source_file by blob_id — full scans of a table that grows
    // with every scanned file until now. sweep_by_priority gains the
    // analyzer prefix and blob_id tiebreak so claim_sweep_items becomes
    // an ordered index walk that stops at LIMIT, instead of
    // scan-and-sorting a queue sized blobs × analyzers.
    "
CREATE INDEX sf_by_blob ON source_file(blob_id);
DROP INDEX sweep_by_priority;
CREATE INDEX sweep_by_priority ON sweep_queue(analyzer, priority DESC, enqueued_at, blob_id);
",
    // v5 → v6 (D92): the claim-gate admission table for non-resident
    // sweep items. Cache-grade scheduling state — refresh regrows it
    // from the grounding fixpoint each sweep wake.
    "
CREATE TABLE sweep_absent_eligible (
  blob_id INTEGER PRIMARY KEY REFERENCES blob(blob_id)
) STRICT, WITHOUT ROWID;
",
];

/// state.db gets REAL migrations forever: an older file is upgraded in
/// place by [`STATE_MIGRATIONS`], never dropped. A newer-than-supported
/// version is a hard error in both files (no downgrades).
/// v2: invite.role + view_grant (auth v1, D30/D68).
/// v3: job (D74 durable job history — session-precedent: authoritative
/// but snapshot-excluded).
pub const STATE_SCHEMA_VERSION: u32 = 3;

/// The state.db migration ladder: `STATE_MIGRATIONS[i]` is the SQL batch
/// migrating version `i + 1` to `i + 2`; each step runs in its own
/// transaction and stamps `user_version` before the next. Every entry is
/// append-only and immutable once released — editing a shipped step
/// would fork the upgrade path of existing deployments.
///
/// Writing a step: additive DDL (ADD COLUMN, CREATE TABLE) plus any row
/// backfill. Anything a snapshot round-trips (D37) must keep
/// round-tripping after the step — the snapshot codec is the cross-check
/// that a migration didn't silently change state semantics.
pub const STATE_MIGRATIONS: &[&str] = &[
    // v1 → v2 (D68): invites carry the role they mint, and the friend
    // surface is a per-user view ACL. The ALTER needs a DEFAULT for any
    // pre-existing rows; 1 = friend (least privilege). Writers always
    // set role explicitly.
    "
ALTER TABLE invite ADD COLUMN role INTEGER NOT NULL DEFAULT 1;
CREATE TABLE view_grant (
  user_id   INTEGER NOT NULL REFERENCES user(user_id),
  view_name TEXT NOT NULL,
  PRIMARY KEY (user_id, view_name)
) STRICT;
",
    // v2 → v3 (D74): the durable job ledger. Terminal snapshots only —
    // the in-memory registry remains the live surface; `detail` is the
    // wire JobDetail JSON frozen at finish (a parse miss on a future
    // shape renders a stub row from the columns, never an error).
    "
CREATE TABLE job (
  job_id      INTEGER PRIMARY KEY,
  kind        INTEGER NOT NULL,
  name        TEXT NOT NULL,
  state       INTEGER NOT NULL,
  started_at  INTEGER NOT NULL,
  finished_at INTEGER,
  detail      BLOB
) STRICT;
",
];

/// `application_id` magics: "dtbc" / "dtbs" as big-endian ASCII.
pub const CACHE_APP_ID: u32 = 0x6474_6263;
pub const STATE_APP_ID: u32 = 0x6474_6273;

/// Parent tables precede children so FK references resolve at DML time.
pub const CACHE_DDL: &str = r"
CREATE TABLE blob (
  blob_id       INTEGER PRIMARY KEY,
  hash          BLOB NOT NULL,
  size          INTEGER,
  namespace     INTEGER NOT NULL DEFAULT 0,
  residency     INTEGER NOT NULL,
  verified_at   INTEGER,
  obao          INTEGER NOT NULL DEFAULT 0,
  last_access   INTEGER,
  pinned_reason INTEGER
) STRICT;
CREATE UNIQUE INDEX blob_hash ON blob(hash);
CREATE INDEX blob_residency ON blob(residency) WHERE residency != 2;

CREATE TABLE alias (
  algo    INTEGER NOT NULL,
  digest  BLOB NOT NULL,
  blob_id INTEGER NOT NULL REFERENCES blob(blob_id),
  PRIMARY KEY (algo, digest, blob_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX alias_by_blob ON alias(blob_id);

CREATE TABLE recipe (
  recipe_id   INTEGER PRIMARY KEY,
  blob_id     INTEGER NOT NULL UNIQUE REFERENCES blob(blob_id),
  op_kind     INTEGER NOT NULL,
  op_name     TEXT NOT NULL,
  seek_class  INTEGER NOT NULL,
  verify      INTEGER NOT NULL DEFAULT 0,
  verified_at INTEGER,
  fail_error  TEXT,
  fail_peer   BLOB,
  source      INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE recipe_input (
  recipe_id INTEGER NOT NULL REFERENCES recipe(recipe_id),
  position  INTEGER NOT NULL,
  blob_id   INTEGER NOT NULL REFERENCES blob(blob_id),
  role      TEXT,
  PRIMARY KEY (recipe_id, position)
) STRICT, WITHOUT ROWID;
CREATE INDEX rin_by_blob ON recipe_input(blob_id);

CREATE TABLE recipe_output (
  recipe_id INTEGER NOT NULL REFERENCES recipe(recipe_id),
  ordinal   INTEGER NOT NULL,
  blob_id   INTEGER NOT NULL REFERENCES blob(blob_id),
  size      INTEGER NOT NULL,
  name      TEXT,
  PRIMARY KEY (recipe_id, ordinal)
) STRICT, WITHOUT ROWID;
CREATE INDEX rout_by_blob ON recipe_output(blob_id);

CREATE TABLE source_file (
  path       TEXT PRIMARY KEY,
  mtime_ns   INTEGER NOT NULL,
  size       INTEGER NOT NULL,
  blob_id    INTEGER REFERENCES blob(blob_id),
  scanned_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX sf_by_blob ON source_file(blob_id);

CREATE TABLE detector (
  detector_id INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  blob_id     INTEGER NOT NULL REFERENCES blob(blob_id),
  rules       BLOB NOT NULL
) STRICT;

CREATE TABLE dat_source (
  source_id           INTEGER PRIMARY KEY,
  provider            TEXT NOT NULL,
  system              TEXT NOT NULL,
  current_revision_id INTEGER,
  UNIQUE (provider, system)
) STRICT;

CREATE TABLE dat_revision (
  revision_id  INTEGER PRIMARY KEY,
  source_id    INTEGER NOT NULL REFERENCES dat_source(source_id),
  blob_id      INTEGER NOT NULL REFERENCES blob(blob_id),
  format       INTEGER NOT NULL,
  version      TEXT,
  dat_date     TEXT,
  header       BLOB,
  detector_id  INTEGER REFERENCES detector(detector_id),
  imported_at  INTEGER NOT NULL,
  materialized INTEGER NOT NULL DEFAULT 1
) STRICT;

CREATE TABLE content_identity (
  identity_id INTEGER PRIMARY KEY,
  size        INTEGER,
  crc32       BLOB,
  md5         BLOB,
  sha1        BLOB,
  sha256      BLOB,
  strength    INTEGER NOT NULL
) STRICT;
CREATE INDEX ci_sha1 ON content_identity(sha1) WHERE sha1 IS NOT NULL;
CREATE INDEX ci_md5 ON content_identity(md5) WHERE md5 IS NOT NULL;
CREATE INDEX ci_sha256 ON content_identity(sha256) WHERE sha256 IS NOT NULL;
CREATE INDEX ci_crc ON content_identity(crc32, size) WHERE crc32 IS NOT NULL;

CREATE TABLE identity_blob (
  identity_id INTEGER NOT NULL REFERENCES content_identity(identity_id),
  blob_id     INTEGER NOT NULL REFERENCES blob(blob_id),
  basis       INTEGER NOT NULL,
  PRIMARY KEY (identity_id, blob_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX ib_by_blob ON identity_blob(blob_id);

CREATE TABLE entry (
  entry_id      INTEGER PRIMARY KEY,
  revision_id   INTEGER NOT NULL REFERENCES dat_revision(revision_id),
  name          TEXT NOT NULL,
  stable_key    TEXT,
  description   TEXT,
  year          TEXT,
  manufacturer  TEXT,
  is_bios       INTEGER NOT NULL DEFAULT 0,
  is_device     INTEGER NOT NULL DEFAULT 0,
  is_mechanical INTEGER NOT NULL DEFAULT 0,
  runnable      INTEGER NOT NULL DEFAULT 1,
  cloneof       TEXT,
  romof         TEXT,
  sampleof      TEXT,
  cloneof_id    INTEGER,
  romof_id      INTEGER,
  attrs         BLOB,
  UNIQUE (revision_id, name)
) STRICT;
CREATE INDEX entry_stable ON entry(revision_id, stable_key);

CREATE TABLE release (
  entry_id   INTEGER NOT NULL REFERENCES entry(entry_id),
  name       TEXT NOT NULL,
  region     TEXT NOT NULL,
  language   TEXT,
  rel_date   TEXT,
  is_default INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE rom_claim (
  claim_id    INTEGER PRIMARY KEY,
  entry_id    INTEGER NOT NULL REFERENCES entry(entry_id),
  kind        INTEGER NOT NULL,
  name        TEXT NOT NULL,
  size        INTEGER,
  crc32       BLOB,
  md5         BLOB,
  sha1        BLOB,
  sha256      BLOB,
  status      INTEGER NOT NULL DEFAULT 0,
  mia         INTEGER NOT NULL DEFAULT 0,
  optional    INTEGER NOT NULL DEFAULT 0,
  merge_name  TEXT,
  identity_id INTEGER REFERENCES content_identity(identity_id),
  attrs       BLOB
) STRICT;
CREATE INDEX claim_by_entry ON rom_claim(entry_id);
CREATE INDEX claim_by_identity ON rom_claim(identity_id);

CREATE TABLE annotation (
  entry_id INTEGER NOT NULL REFERENCES entry(entry_id),
  layer    TEXT NOT NULL,
  data     BLOB NOT NULL,
  PRIMARY KEY (entry_id, layer)
) STRICT, WITHOUT ROWID;

CREATE TABLE identity_status (
  identity_id INTEGER PRIMARY KEY REFERENCES content_identity(identity_id),
  state       INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE entry_audit (
  entry_id      INTEGER PRIMARY KEY REFERENCES entry(entry_id),
  required      INTEGER NOT NULL,
  have_verified INTEGER NOT NULL,
  have_claimed  INTEGER NOT NULL,
  peer_avail    INTEGER NOT NULL,
  missing       INTEGER NOT NULL,
  computed_at   INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- D49 rule 3: component hashes whose *seek path* produced bytes that
-- failed output-bao verification. Sequential replay is unaffected; the
-- planner serves reads through the known-good sequential path (spill) for
-- these until a fixed component ships under a new hash. Cache-grade on
-- purpose: losing the row costs one more detected-and-refused bad serve,
-- never wrong bytes (the per-read bao check is what actually protects).
CREATE TABLE seek_quarantine (
  component      BLOB PRIMARY KEY,
  quarantined_at INTEGER NOT NULL,
  reason         TEXT NOT NULL
) STRICT, WITHOUT ROWID;

-- D45/D48: analyzer provenance, INCLUDING negative results — which
-- analyzer identity ran over which bytes and what it concluded. Pure
-- function of bytes × analyzer, so cache-grade; batched into signed
-- snapshots so bare-NAS recovery doesn't re-pay expensive negatives.
CREATE TABLE analysis (
  blob_id     INTEGER NOT NULL REFERENCES blob(blob_id),
  analyzer    BLOB NOT NULL,
  outcome     INTEGER NOT NULL,
  detail      TEXT,
  analyzed_at INTEGER NOT NULL,
  PRIMARY KEY (blob_id, analyzer)
) STRICT, WITHOUT ROWID;
CREATE INDEX analysis_by_analyzer ON analysis(analyzer);

-- D45/D47: the refinement sweep queue. Scheduling state only (never
-- truth): rows are (candidate × analyzer) pairs awaiting a sweep, with
-- dat-aware priority allowed by D47 (claims stay dat-blind; ordering may
-- not). leased_until (D71): a claimed item is invisible to other
-- workers until the lease expires — dedup across the daemon worker and
-- CLI sweeps, never a correctness gate (at-least-once absorbs stale
-- leases; 0 = unleased).
CREATE TABLE sweep_queue (
  blob_id      INTEGER NOT NULL REFERENCES blob(blob_id),
  analyzer     BLOB NOT NULL,
  priority     INTEGER NOT NULL DEFAULT 0,
  enqueued_at  INTEGER NOT NULL,
  leased_until INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (blob_id, analyzer)
) STRICT, WITHOUT ROWID;
CREATE INDEX sweep_by_priority ON sweep_queue(analyzer, priority DESC, enqueued_at, blob_id);

-- D92: analyzers consume the logical CAS — the claim gate hands out
-- non-resident queue items only when this table admits them. Regrown
-- once per sweep wake ([`Db::refresh_absent_eligibility`]) from the
-- D21 grounding fixpoint ∩ the molten eagerness policy
-- (refine:absent:mode); cache-grade by construction.
CREATE TABLE sweep_absent_eligible (
  blob_id INTEGER PRIMARY KEY REFERENCES blob(blob_id)
) STRICT, WITHOUT ROWID;

-- D72: the eviction singleton guard — the one lease that IS a
-- correctness gate (two concurrent grounding computations can jointly
-- approve stranding a mutually-inverse recipe pair). Single seeded row;
-- claims are atomic UPDATEs under WAL. Never conflate with the D71
-- sweep leases (those are dedup).
CREATE TABLE gc_guard (
  guard_id   INTEGER PRIMARY KEY CHECK (guard_id = 1),
  holder     BLOB,
  expires_at INTEGER NOT NULL DEFAULT 0
) STRICT;
INSERT INTO gc_guard (guard_id, holder, expires_at) VALUES (1, NULL, 0);

-- D73: orphan candidate marks. Cache-grade scheduling/review state:
-- first-observed-unreferenced is the grace clock, and every sweep
-- clears marks that anything now roots. Deletion NEVER reads this
-- table alone — apply re-verifies reachability at delete time.
CREATE TABLE orphan_candidate (
  blob_id   INTEGER PRIMARY KEY REFERENCES blob(blob_id),
  marked_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE peer (
  peer_id   INTEGER PRIMARY KEY,
  node_id   BLOB NOT NULL UNIQUE,
  name      TEXT,
  last_seen INTEGER
) STRICT;

CREATE TABLE peer_have (
  peer_id INTEGER NOT NULL REFERENCES peer(peer_id),
  blob_id INTEGER NOT NULL REFERENCES blob(blob_id),
  seen_at INTEGER NOT NULL,
  PRIMARY KEY (peer_id, blob_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX ph_by_blob ON peer_have(blob_id);
";

/// Truncation order for cache rebuild: children before parents (FKs on).
/// `gc_guard` is deliberately absent: its single seeded row must exist
/// for claims to UPDATE, and a stale holder is already handled by TTL.
pub const CACHE_TABLES_CHILD_FIRST: &[&str] = &[
    "sweep_absent_eligible",
    "orphan_candidate",
    "sweep_queue",
    "analysis",
    "seek_quarantine",
    "entry_audit",
    "identity_status",
    "annotation",
    "rom_claim",
    "release",
    "entry",
    "identity_blob",
    "content_identity",
    "dat_revision",
    "dat_source",
    "detector",
    "peer_have",
    "peer",
    "source_file",
    "recipe_output",
    "recipe_input",
    "recipe",
    "alias",
    "blob",
];

pub const STATE_DDL: &str = r#"
CREATE TABLE tag (
  name       TEXT PRIMARY KEY,
  hash       BLOB NOT NULL,
  created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE user (
  user_id    INTEGER PRIMARY KEY,
  username   TEXT NOT NULL UNIQUE,
  argon2     TEXT NOT NULL,
  role       INTEGER NOT NULL,
  created_at INTEGER NOT NULL
) STRICT;

-- Single-use, role-carrying, expiring (D68). token_hash is
-- blake3(token): a stolen state.db mints nothing. The DEFAULT on role
-- exists only for the v1→v2 ALTER; writers always set it explicitly.
CREATE TABLE invite (
  token_hash BLOB PRIMARY KEY,
  created_by INTEGER REFERENCES user(user_id),
  expires_at INTEGER NOT NULL,
  used_by    INTEGER,
  role       INTEGER NOT NULL DEFAULT 1
) STRICT;

-- Authoritative but truncatable; excluded from CAS snapshots.
CREATE TABLE session (
  token_hash BLOB PRIMARY KEY,
  user_id    INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
) STRICT;

-- The friend-surface ACL (D68): owners see everything; friends see
-- exactly the views granted here (list, browse, download).
CREATE TABLE view_grant (
  user_id   INTEGER NOT NULL REFERENCES user(user_id),
  view_name TEXT NOT NULL,
  PRIMARY KEY (user_id, view_name)
) STRICT;

CREATE TABLE peer_acl (
  node_id BLOB PRIMARY KEY,
  label   TEXT,
  granted INTEGER NOT NULL
) STRICT;

CREATE TABLE view_def (
  name       TEXT PRIMARY KEY,
  definition BLOB NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE channel (
  name      TEXT PRIMARY KEY,
  kind      INTEGER NOT NULL,
  promotion INTEGER NOT NULL,
  head_hash BLOB,
  seq       INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE subscription (
  peer_node   BLOB NOT NULL,
  channel     TEXT NOT NULL,
  policy      INTEGER NOT NULL,
  pinned_head BLOB,
  PRIMARY KEY (peer_node, channel)
) STRICT;

CREATE TABLE config (
  key   TEXT PRIMARY KEY,
  value BLOB NOT NULL
) STRICT;

-- D74: durable job history. The session precedent: authoritative but
-- truncatable, EXCLUDED from CAS snapshots (history is not recovery
-- truth). Rows are terminal snapshots — inserted running, finalized
-- once; a row still `running` at daemon startup is crash evidence and
-- gets marked interrupted. kind/state codes: JobKind/JobState
-- (types.rs).
CREATE TABLE job (
  job_id      INTEGER PRIMARY KEY,
  kind        INTEGER NOT NULL,
  name        TEXT NOT NULL,
  state       INTEGER NOT NULL,
  started_at  INTEGER NOT NULL,
  finished_at INTEGER,
  detail      BLOB
) STRICT;

CREATE TABLE snapshot_log (
  seq        INTEGER PRIMARY KEY,
  hash       BLOB NOT NULL,
  created_at INTEGER NOT NULL
) STRICT;
"#;
