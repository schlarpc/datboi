//! DDL for both database files (docs/65-schema.md §1–§4).
//!
//! cache.db tables are derivable caches (D15): any of them may be dropped
//! and rebuilt from CAS bytes + deterministic re-import. state.db tables
//! are authoritative until snapshotted into CAS (D37). Tables are STRICT;
//! hashes are 32-byte BLOBs; graph tables use integer surrogate keys.

/// Bumped only on incompatible schema changes. cache.db migration policy
/// is "drop and rebuild"; state.db gets real migrations forever (D37).
pub const SCHEMA_VERSION: u32 = 1;

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
pub const CACHE_TABLES_CHILD_FIRST: &[&str] = &[
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

CREATE TABLE invite (
  token_hash BLOB PRIMARY KEY,
  created_by INTEGER REFERENCES user(user_id),
  expires_at INTEGER NOT NULL,
  used_by    INTEGER
) STRICT;

-- Authoritative but truncatable; excluded from CAS snapshots.
CREATE TABLE session (
  token_hash BLOB PRIMARY KEY,
  user_id    INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
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

CREATE TABLE snapshot_log (
  seq        INTEGER PRIMARY KEY,
  hash       BLOB NOT NULL,
  created_at INTEGER NOT NULL
) STRICT;
"#;
