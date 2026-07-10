//! State snapshots and alias batches (D15/D22, docs/10-cas.md recovery
//! step 3): the signed recovery root that lets `recover` rebuild catalog
//! typing and the alias table without a full re-derive.
//!
//! Two object kinds:
//!
//! * `datboi/aliases/1\n` — one **alias batch**: rows of the full hash
//!   tuple, strictly ascending by member blake3 so identical content always
//!   encodes to identical bytes (content-address dedup across snapshots).
//! * `datboi/statesnap/1\n` — the **snapshot**: an ed25519-signed envelope
//!   over a payload carrying dat-source typing (enough to replay
//!   `dat import` from CAS blobs) and references to sharded alias batches.
//!
//! Sharding (ratified 2026-07-06): the snapshot stays tiny and references
//! `alias_fanout` batch blobs; a steady-state snapshot only mints new bytes
//! for shards that actually changed — unchanged shards dedupe by hash. The
//! shard *assignment* ([`alias_shard`]) is encoder policy, not format: a
//! decoder loads whatever batches the snapshot lists. Changing the fanout
//! between snapshots is legal; it just forfeits one round of dedup.
//!
//! Unlike recipes, snapshot identities need not be stable forever — only
//! the latest snapshot matters — but the format is still strict canonical
//! CBOR with golden-vector coverage, because a recovery root that can't be
//! parsed decades later is no root at all.

use crate::alias::AliasTuple;
use crate::cbor::{self, Value};
use crate::hash::Blake3;
use crate::identity::{self, Identity, PublicKey};
use crate::object::{self, ObjectKind};

pub const ALIASES_VERSION: u32 = 1;
pub const ANALYSIS_VERSION: u32 = 1;
pub const STATESNAP_VERSION: u32 = 1;
const ALIASES_HEADER: &[u8] = b"datboi/aliases/1\n";
const ANALYSIS_HEADER: &[u8] = b"datboi/analysis/1\n";
const STATESNAP_HEADER: &[u8] = b"datboi/statesnap/1\n";

/// Highest permitted alias fanout: one shard per leading-byte value.
pub const MAX_ALIAS_FANOUT: usize = 256;

// alias batch: {1: rows}; row {1: blake3, 2: size, 3: crc32, 4: md5,
// 5: sha1, 6: sha256}.
const BATCHKEY_ROWS: u64 = 1;
const ROWKEY_BLAKE3: u64 = 1;
const ROWKEY_SIZE: u64 = 2;
const ROWKEY_CRC32: u64 = 3;
const ROWKEY_MD5: u64 = 4;
const ROWKEY_SHA1: u64 = 5;
const ROWKEY_SHA256: u64 = 6;

// analysis batch: {1: rows}; row {1: blob blake3, 2: analyzer blake3,
// 3: outcome (0 negative / 1 positive), ?4: detail text}.
const ANKEY_ROWS: u64 = 1;
const ANROW_BLOB: u64 = 1;
const ANROW_ANALYZER: u64 = 2;
const ANROW_OUTCOME: u64 = 3;
const ANROW_DETAIL: u64 = 4;

// statesnap envelope: {1: payload bstr, 2: public key, 3: signature}.
const ENVKEY_PAYLOAD: u64 = 1;
const ENVKEY_PUBLIC_KEY: u64 = 2;
const ENVKEY_SIGNATURE: u64 = 3;

// payload: {1: sequence, 2: created_at, 3: sources, 4: alias_fanout,
// 5: alias_batches, ?6: analysis_fanout, ?7: analysis_batches}; source
// {1: provider, 2: system, 3: dat blob, 4: imported_at}. Keys 6/7 are
// omitted together when no analysis provenance exists (one encoding per
// value: a present-but-zero fanout is rejected) — pre-D48 snapshots
// decode unchanged.
const PAYKEY_SEQUENCE: u64 = 1;
const PAYKEY_CREATED_AT: u64 = 2;
const PAYKEY_SOURCES: u64 = 3;
const PAYKEY_ALIAS_FANOUT: u64 = 4;
const PAYKEY_ALIAS_BATCHES: u64 = 5;
const PAYKEY_ANALYSIS_FANOUT: u64 = 6;
const PAYKEY_ANALYSIS_BATCHES: u64 = 7;
// Additive keys 8/9 (2026-07-09): tags + config KV ride the snapshot so
// recovery keeps views (defs are config rows, the flip is a tag). Both
// are inline — dozens of tiny rows, unlike the sharded alias batches.
const PAYKEY_TAGS: u64 = 8;
const PAYKEY_CONFIG: u64 = 9;
const TAGKEY_NAME: u64 = 1;
const TAGKEY_HASH: u64 = 2;
const CFGKEY_KEY: u64 = 1;
const CFGKEY_VALUE: u64 = 2;
const SRCKEY_PROVIDER: u64 = 1;
const SRCKEY_SYSTEM: u64 = 2;
const SRCKEY_DAT_BLOB: u64 = 3;
const SRCKEY_IMPORTED_AT: u64 = 4;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SnapshotError {
    #[error("not a datboi {0} object")]
    WrongKind(&'static str),
    #[error("unsupported {0} version {1}")]
    Version(&'static str, u32),
    #[error(transparent)]
    Cbor(#[from] cbor::DecodeError),
    #[error("invalid snapshot structure: {0}")]
    Invalid(&'static str),
    #[error("snapshot signature verification failed")]
    BadSignature,
}

/// Which shard of `fanout` an alias row belongs to. Range partition on the
/// leading blake3 byte: monotone, works for any fanout in `1..=256`, and
/// stays stable as long as the encoder keeps the same fanout.
///
/// # Panics
/// If `fanout` is outside `1..=256`.
#[must_use]
pub fn alias_shard(hash: &Blake3, fanout: usize) -> usize {
    assert!(
        (1..=MAX_ALIAS_FANOUT).contains(&fanout),
        "fanout must be 1..=256"
    );
    usize::from(hash.0[0]) * fanout / 256
}

/// One alias batch: full hash-tuple rows, strictly ascending by blake3.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AliasBatch {
    pub rows: Vec<AliasTuple>,
}

impl AliasBatch {
    /// Encode to canonical object bytes. Rows are sorted by blake3 here so
    /// callers can't produce two encodings of the same set; duplicate
    /// blake3s are rejected (one bytes → one tuple, by construction).
    pub fn encode(&self) -> Result<Vec<u8>, SnapshotError> {
        let mut rows = self.rows.clone();
        rows.sort_by_key(|r| r.blake3);
        if rows.windows(2).any(|w| w[0].blake3 == w[1].blake3) {
            return Err(SnapshotError::Invalid("duplicate blake3 in alias batch"));
        }
        let body = cbor::encode(&Value::Map(vec![(
            BATCHKEY_ROWS,
            Value::Array(rows.iter().map(row_to_value).collect()),
        )]))
        .expect("single constant key");
        let mut out = Vec::with_capacity(ALIASES_HEADER.len() + body.len());
        out.extend_from_slice(ALIASES_HEADER);
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SnapshotError> {
        let body = expect_object(bytes, ObjectKind::AliasBatch, ALIASES_VERSION, "aliases")?;
        let value = cbor::decode(body)?;
        let mut rows = None;
        for (key, val) in as_map(&value)? {
            match *key {
                BATCHKEY_ROWS => {
                    let Value::Array(items) = val else {
                        return Err(SnapshotError::Invalid("rows must be an array"));
                    };
                    rows = Some(
                        items
                            .iter()
                            .map(row_from_value)
                            .collect::<Result<Vec<_>, _>>()?,
                    );
                }
                _ => return Err(SnapshotError::Invalid("unknown alias batch key")),
            }
        }
        let rows: Vec<AliasTuple> = rows.ok_or(SnapshotError::Invalid("missing rows"))?;
        if rows.windows(2).any(|w| w[0].blake3 >= w[1].blake3) {
            return Err(SnapshotError::Invalid("alias rows not strictly ascending"));
        }
        Ok(Self { rows })
    }
}

/// One analyzer-provenance fact (D48): what `analyzer` (identity hash —
/// a component hash for wasm analyzers, a tagged version hash for native
/// ones) concluded about `blob`'s bytes. Negative results are the whole
/// point: recovery must not re-pay expensive analysis that found nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisRow {
    pub blob: Blake3,
    pub analyzer: Blake3,
    /// `false` = analyzed, nothing found; `true` = discovery (recipes or
    /// claims were minted — those live as ordinary CAS objects).
    pub positive: bool,
    /// Optional analyzer-owned annotation (why negative, what was found).
    pub detail: Option<String>,
}

/// One analysis batch: rows strictly ascending by (blob, analyzer).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnalysisBatch {
    pub rows: Vec<AnalysisRow>,
}

impl AnalysisBatch {
    /// Encode to canonical object bytes (sorted here; duplicates rejected).
    pub fn encode(&self) -> Result<Vec<u8>, SnapshotError> {
        let mut rows = self.rows.clone();
        rows.sort_by_key(|r| (r.blob, r.analyzer));
        if rows
            .windows(2)
            .any(|w| (w[0].blob, w[0].analyzer) == (w[1].blob, w[1].analyzer))
        {
            return Err(SnapshotError::Invalid(
                "duplicate (blob, analyzer) in analysis batch",
            ));
        }
        if rows
            .iter()
            .any(|r| r.detail.as_ref().is_some_and(String::is_empty))
        {
            return Err(SnapshotError::Invalid("empty analysis detail"));
        }
        let body = cbor::encode(&Value::Map(vec![(
            ANKEY_ROWS,
            Value::Array(rows.iter().map(analysis_row_to_value).collect()),
        )]))
        .expect("single constant key");
        let mut out = Vec::with_capacity(ANALYSIS_HEADER.len() + body.len());
        out.extend_from_slice(ANALYSIS_HEADER);
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SnapshotError> {
        let body = expect_object(
            bytes,
            ObjectKind::AnalysisBatch,
            ANALYSIS_VERSION,
            "analysis",
        )?;
        let value = cbor::decode(body)?;
        let mut rows = None;
        for (key, val) in as_map(&value)? {
            match *key {
                ANKEY_ROWS => {
                    let Value::Array(items) = val else {
                        return Err(SnapshotError::Invalid("rows must be an array"));
                    };
                    rows = Some(
                        items
                            .iter()
                            .map(analysis_row_from_value)
                            .collect::<Result<Vec<_>, _>>()?,
                    );
                }
                _ => return Err(SnapshotError::Invalid("unknown analysis batch key")),
            }
        }
        let rows: Vec<AnalysisRow> = rows.ok_or(SnapshotError::Invalid("missing rows"))?;
        if rows
            .windows(2)
            .any(|w| (w[0].blob, w[0].analyzer) >= (w[1].blob, w[1].analyzer))
        {
            return Err(SnapshotError::Invalid(
                "analysis rows not strictly ascending",
            ));
        }
        Ok(Self { rows })
    }
}

/// A dat source reference: everything `recover` needs to replay
/// `dat import` of the current revision from its CAS blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRef {
    pub provider: String,
    pub system: String,
    /// blake3 of the dat file blob (in `data/` — dats are opaque payloads).
    pub dat_blob: Blake3,
    /// Original import wall-clock (unix seconds), carried so a replayed
    /// import reproduces identical rows.
    pub imported_at: u64,
}

/// The signed snapshot payload. Encode with [`SnapshotPayload::encode_signed`];
/// read back with [`StateSnapshot::decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPayload {
    /// Monotonic per-instance sequence (state.db snapshot_log).
    pub sequence: u64,
    /// Unix seconds.
    pub created_at: u64,
    /// Sorted by (provider, system), unique.
    pub sources: Vec<SourceRef>,
    /// Number of alias shards; `alias_batches.len() == alias_fanout`.
    pub alias_fanout: usize,
    /// blake3 of the `datboi/aliases/1` batch blob for each shard, indexed
    /// by [`alias_shard`]. Empty shards still reference a (tiny, shared)
    /// empty-batch blob — fixed-length keeps the format shape trivial.
    pub alias_batches: Vec<Blake3>,
    /// Analysis-provenance shards (D48), same discipline as aliases;
    /// sharded by the row's *blob* hash via [`alias_shard`]. Zero fanout
    /// (with an empty batch list) means "no provenance in this snapshot"
    /// and encodes as the fields being absent.
    pub analysis_fanout: usize,
    pub analysis_batches: Vec<Blake3>,
    /// Tags (name → object hash), sorted by name, unique. Carries the
    /// `view/<name>` flips (D33) through recovery.
    pub tags: Vec<(String, Blake3)>,
    /// Authoritative config KV (view definitions live here), sorted by
    /// key, unique.
    pub config: Vec<(String, Vec<u8>)>,
}

impl SnapshotPayload {
    fn validate(&self) -> Result<(), SnapshotError> {
        if !(1..=MAX_ALIAS_FANOUT).contains(&self.alias_fanout) {
            return Err(SnapshotError::Invalid("alias fanout must be 1..=256"));
        }
        if self.alias_batches.len() != self.alias_fanout {
            return Err(SnapshotError::Invalid("alias batch count != fanout"));
        }
        if self.analysis_fanout > MAX_ALIAS_FANOUT {
            return Err(SnapshotError::Invalid("analysis fanout must be 0..=256"));
        }
        if self.analysis_batches.len() != self.analysis_fanout {
            return Err(SnapshotError::Invalid("analysis batch count != fanout"));
        }
        let sorted = self
            .sources
            .windows(2)
            .all(|w| (&w[0].provider, &w[0].system) < (&w[1].provider, &w[1].system));
        if !sorted {
            return Err(SnapshotError::Invalid(
                "sources not sorted by provider/system",
            ));
        }
        if self
            .sources
            .iter()
            .any(|s| s.provider.is_empty() || s.system.is_empty())
        {
            return Err(SnapshotError::Invalid("empty source provider or system"));
        }
        if !self.tags.windows(2).all(|w| w[0].0 < w[1].0) {
            return Err(SnapshotError::Invalid("tags not sorted/unique by name"));
        }
        if self.tags.iter().any(|(name, _)| name.is_empty()) {
            return Err(SnapshotError::Invalid("empty tag name"));
        }
        if !self.config.windows(2).all(|w| w[0].0 < w[1].0) {
            return Err(SnapshotError::Invalid("config not sorted/unique by key"));
        }
        if self.config.iter().any(|(key, _)| key.is_empty()) {
            return Err(SnapshotError::Invalid("empty config key"));
        }
        Ok(())
    }

    fn payload_bytes(&self) -> Result<Vec<u8>, SnapshotError> {
        self.validate()?;
        let sources = self.sources.iter().map(source_to_value).collect();
        let batches = self
            .alias_batches
            .iter()
            .map(|h| Value::Bytes(h.0.to_vec()))
            .collect();
        let mut entries = vec![
            (PAYKEY_SEQUENCE, Value::Uint(self.sequence)),
            (PAYKEY_CREATED_AT, Value::Uint(self.created_at)),
            (PAYKEY_SOURCES, Value::Array(sources)),
            (PAYKEY_ALIAS_FANOUT, Value::Uint(self.alias_fanout as u64)),
            (PAYKEY_ALIAS_BATCHES, Value::Array(batches)),
        ];
        if self.analysis_fanout > 0 {
            entries.push((
                PAYKEY_ANALYSIS_FANOUT,
                Value::Uint(self.analysis_fanout as u64),
            ));
            entries.push((
                PAYKEY_ANALYSIS_BATCHES,
                Value::Array(
                    self.analysis_batches
                        .iter()
                        .map(|h| Value::Bytes(h.0.to_vec()))
                        .collect(),
                ),
            ));
        }
        // Empty encodes as absence (one encoding per value).
        if !self.tags.is_empty() {
            entries.push((
                PAYKEY_TAGS,
                Value::Array(
                    self.tags
                        .iter()
                        .map(|(name, hash)| {
                            Value::Map(vec![
                                (TAGKEY_NAME, Value::Text(name.clone())),
                                (TAGKEY_HASH, Value::Bytes(hash.0.to_vec())),
                            ])
                        })
                        .collect(),
                ),
            ));
        }
        if !self.config.is_empty() {
            entries.push((
                PAYKEY_CONFIG,
                Value::Array(
                    self.config
                        .iter()
                        .map(|(key, value)| {
                            Value::Map(vec![
                                (CFGKEY_KEY, Value::Text(key.clone())),
                                (CFGKEY_VALUE, Value::Bytes(value.clone())),
                            ])
                        })
                        .collect(),
                ),
            ));
        }
        Ok(cbor::encode(&Value::Map(entries)).expect("field keys are distinct constants"))
    }

    /// Encode and sign: the signature covers `header || payload` bytes
    /// exactly as they appear in the blob (the header provides domain
    /// separation and binds the format version into the signature).
    pub fn encode_signed(&self, identity: &Identity) -> Result<Vec<u8>, SnapshotError> {
        let payload = self.payload_bytes()?;
        let mut msg = Vec::with_capacity(STATESNAP_HEADER.len() + payload.len());
        msg.extend_from_slice(STATESNAP_HEADER);
        msg.extend_from_slice(&payload);
        let signature = identity.sign(&msg);
        let body = cbor::encode(&Value::Map(vec![
            (ENVKEY_PAYLOAD, Value::Bytes(payload)),
            (
                ENVKEY_PUBLIC_KEY,
                Value::Bytes(identity.public_key().to_vec()),
            ),
            (ENVKEY_SIGNATURE, Value::Bytes(signature.to_vec())),
        ]))
        .expect("field keys are distinct constants");
        let mut out = Vec::with_capacity(STATESNAP_HEADER.len() + body.len());
        out.extend_from_slice(STATESNAP_HEADER);
        out.extend_from_slice(&body);
        Ok(out)
    }
}

/// A decoded snapshot. `decode` checks structure and that the embedded
/// signature verifies under the embedded key; [`StateSnapshot::verify`]
/// additionally pins the key to the caller's expected identity — recovery
/// MUST do both (an attacker who can write meta/ can mint self-consistent
/// snapshots under their own key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    pub payload: SnapshotPayload,
    pub public_key: PublicKey,
}

impl StateSnapshot {
    pub fn decode(bytes: &[u8]) -> Result<Self, SnapshotError> {
        let body = expect_object(
            bytes,
            ObjectKind::StateSnapshot,
            STATESNAP_VERSION,
            "statesnap",
        )?;
        let value = cbor::decode(body)?;
        let (mut payload, mut public_key, mut signature) = (None, None, None);
        for (key, val) in as_map(&value)? {
            match *key {
                ENVKEY_PAYLOAD => payload = Some(as_bytes(val)?.to_vec()),
                ENVKEY_PUBLIC_KEY => {
                    public_key = Some(as_fixed::<32>(val, "public key must be 32 bytes")?);
                }
                ENVKEY_SIGNATURE => {
                    signature = Some(as_fixed::<64>(val, "signature must be 64 bytes")?);
                }
                _ => return Err(SnapshotError::Invalid("unknown envelope key")),
            }
        }
        let payload = payload.ok_or(SnapshotError::Invalid("missing payload"))?;
        let public_key = public_key.ok_or(SnapshotError::Invalid("missing public key"))?;
        let signature = signature.ok_or(SnapshotError::Invalid("missing signature"))?;

        let mut msg = Vec::with_capacity(STATESNAP_HEADER.len() + payload.len());
        msg.extend_from_slice(STATESNAP_HEADER);
        msg.extend_from_slice(&payload);
        identity::verify(&public_key, &msg, &signature).map_err(|_| SnapshotError::BadSignature)?;

        let decoded = payload_from_bytes(&payload)?;
        decoded.validate()?;
        Ok(Self {
            payload: decoded,
            public_key,
        })
    }

    /// Pin the snapshot to the identity recovery trusts.
    pub fn verify(&self, expected: &PublicKey) -> Result<(), SnapshotError> {
        if &self.public_key == expected {
            Ok(())
        } else {
            Err(SnapshotError::BadSignature)
        }
    }
}

fn payload_from_bytes(bytes: &[u8]) -> Result<SnapshotPayload, SnapshotError> {
    let value = cbor::decode(bytes)?;
    let (mut sequence, mut created_at, mut sources, mut fanout, mut batches) =
        (None, None, None, None, None);
    let (mut analysis_fanout, mut analysis_batches) = (None, None);
    let (mut tags, mut config) = (None, None);
    for (key, val) in as_map(&value)? {
        match *key {
            PAYKEY_SEQUENCE => sequence = Some(as_uint(val)?),
            PAYKEY_CREATED_AT => created_at = Some(as_uint(val)?),
            PAYKEY_SOURCES => {
                let Value::Array(items) = val else {
                    return Err(SnapshotError::Invalid("sources must be an array"));
                };
                sources = Some(
                    items
                        .iter()
                        .map(source_from_value)
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
            PAYKEY_ALIAS_FANOUT => {
                let raw = as_uint(val)?;
                fanout = Some(
                    usize::try_from(raw)
                        .map_err(|_| SnapshotError::Invalid("alias fanout out of range"))?,
                );
            }
            PAYKEY_ALIAS_BATCHES => {
                let Value::Array(items) = val else {
                    return Err(SnapshotError::Invalid("alias batches must be an array"));
                };
                batches = Some(items.iter().map(as_hash).collect::<Result<Vec<_>, _>>()?);
            }
            PAYKEY_ANALYSIS_FANOUT => {
                let raw = as_uint(val)?;
                let parsed = usize::try_from(raw)
                    .map_err(|_| SnapshotError::Invalid("analysis fanout out of range"))?;
                if parsed == 0 {
                    // Zero encodes as absence; present-but-zero would be a
                    // second encoding of the same value.
                    return Err(SnapshotError::Invalid("analysis fanout present but zero"));
                }
                analysis_fanout = Some(parsed);
            }
            PAYKEY_ANALYSIS_BATCHES => {
                let Value::Array(items) = val else {
                    return Err(SnapshotError::Invalid("analysis batches must be an array"));
                };
                analysis_batches = Some(items.iter().map(as_hash).collect::<Result<Vec<_>, _>>()?);
            }
            PAYKEY_TAGS => {
                let Value::Array(items) = val else {
                    return Err(SnapshotError::Invalid("tags must be an array"));
                };
                if items.is_empty() {
                    return Err(SnapshotError::Invalid("tags present but empty"));
                }
                tags = Some(
                    items
                        .iter()
                        .map(tag_from_value)
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
            PAYKEY_CONFIG => {
                let Value::Array(items) = val else {
                    return Err(SnapshotError::Invalid("config must be an array"));
                };
                if items.is_empty() {
                    return Err(SnapshotError::Invalid("config present but empty"));
                }
                config = Some(
                    items
                        .iter()
                        .map(config_from_value)
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
            _ => return Err(SnapshotError::Invalid("unknown payload key")),
        }
    }
    if analysis_fanout.is_some() != analysis_batches.is_some() {
        return Err(SnapshotError::Invalid(
            "analysis fanout and batches must appear together",
        ));
    }
    Ok(SnapshotPayload {
        sequence: sequence.ok_or(SnapshotError::Invalid("missing sequence"))?,
        created_at: created_at.ok_or(SnapshotError::Invalid("missing created_at"))?,
        sources: sources.ok_or(SnapshotError::Invalid("missing sources"))?,
        alias_fanout: fanout.ok_or(SnapshotError::Invalid("missing alias fanout"))?,
        alias_batches: batches.ok_or(SnapshotError::Invalid("missing alias batches"))?,
        analysis_fanout: analysis_fanout.unwrap_or(0),
        analysis_batches: analysis_batches.unwrap_or_default(),
        tags: tags.unwrap_or_default(),
        config: config.unwrap_or_default(),
    })
}

fn tag_from_value(value: &Value) -> Result<(String, Blake3), SnapshotError> {
    let (mut name, mut hash) = (None, None);
    for (key, val) in as_map(value)? {
        match *key {
            TAGKEY_NAME => {
                let Value::Text(v) = val else {
                    return Err(SnapshotError::Invalid("tag name must be text"));
                };
                name = Some(v.clone());
            }
            TAGKEY_HASH => hash = Some(as_hash(val)?),
            _ => return Err(SnapshotError::Invalid("unknown tag key")),
        }
    }
    Ok((
        name.ok_or(SnapshotError::Invalid("tag missing name"))?,
        hash.ok_or(SnapshotError::Invalid("tag missing hash"))?,
    ))
}

fn config_from_value(value: &Value) -> Result<(String, Vec<u8>), SnapshotError> {
    let (mut key_out, mut val_out) = (None, None);
    for (key, val) in as_map(value)? {
        match *key {
            CFGKEY_KEY => {
                let Value::Text(v) = val else {
                    return Err(SnapshotError::Invalid("config key must be text"));
                };
                key_out = Some(v.clone());
            }
            CFGKEY_VALUE => {
                let Value::Bytes(v) = val else {
                    return Err(SnapshotError::Invalid("config value must be bytes"));
                };
                val_out = Some(v.clone());
            }
            _ => return Err(SnapshotError::Invalid("unknown config entry key")),
        }
    }
    Ok((
        key_out.ok_or(SnapshotError::Invalid("config entry missing key"))?,
        val_out.ok_or(SnapshotError::Invalid("config entry missing value"))?,
    ))
}

fn analysis_row_to_value(row: &AnalysisRow) -> Value {
    let mut entries = vec![
        (ANROW_BLOB, Value::Bytes(row.blob.0.to_vec())),
        (ANROW_ANALYZER, Value::Bytes(row.analyzer.0.to_vec())),
        (ANROW_OUTCOME, Value::Uint(u64::from(row.positive))),
    ];
    if let Some(detail) = &row.detail {
        entries.push((ANROW_DETAIL, Value::Text(detail.clone())));
    }
    Value::Map(entries)
}

fn analysis_row_from_value(value: &Value) -> Result<AnalysisRow, SnapshotError> {
    let (mut blob, mut analyzer, mut outcome, mut detail) = (None, None, None, None);
    for (key, val) in as_map(value)? {
        match *key {
            ANROW_BLOB => blob = Some(as_hash(val)?),
            ANROW_ANALYZER => analyzer = Some(as_hash(val)?),
            ANROW_OUTCOME => match as_uint(val)? {
                0 => outcome = Some(false),
                1 => outcome = Some(true),
                _ => return Err(SnapshotError::Invalid("outcome must be 0 or 1")),
            },
            ANROW_DETAIL => {
                let text = as_text(val)?;
                if text.is_empty() {
                    return Err(SnapshotError::Invalid("empty analysis detail"));
                }
                detail = Some(text.to_owned());
            }
            _ => return Err(SnapshotError::Invalid("unknown analysis row key")),
        }
    }
    Ok(AnalysisRow {
        blob: blob.ok_or(SnapshotError::Invalid("row missing blob"))?,
        analyzer: analyzer.ok_or(SnapshotError::Invalid("row missing analyzer"))?,
        positive: outcome.ok_or(SnapshotError::Invalid("row missing outcome"))?,
        detail,
    })
}

fn row_to_value(row: &AliasTuple) -> Value {
    Value::Map(vec![
        (ROWKEY_BLAKE3, Value::Bytes(row.blake3.0.to_vec())),
        (ROWKEY_SIZE, Value::Uint(row.size)),
        (ROWKEY_CRC32, Value::Bytes(row.crc32.to_vec())),
        (ROWKEY_MD5, Value::Bytes(row.md5.to_vec())),
        (ROWKEY_SHA1, Value::Bytes(row.sha1.to_vec())),
        (ROWKEY_SHA256, Value::Bytes(row.sha256.to_vec())),
    ])
}

fn row_from_value(value: &Value) -> Result<AliasTuple, SnapshotError> {
    let (mut blake3, mut size, mut crc32, mut md5, mut sha1, mut sha256) =
        (None, None, None, None, None, None);
    for (key, val) in as_map(value)? {
        match *key {
            ROWKEY_BLAKE3 => blake3 = Some(as_hash(val)?),
            ROWKEY_SIZE => size = Some(as_uint(val)?),
            ROWKEY_CRC32 => crc32 = Some(as_fixed::<4>(val, "crc32 must be 4 bytes")?),
            ROWKEY_MD5 => md5 = Some(as_fixed::<16>(val, "md5 must be 16 bytes")?),
            ROWKEY_SHA1 => sha1 = Some(as_fixed::<20>(val, "sha1 must be 20 bytes")?),
            ROWKEY_SHA256 => sha256 = Some(as_fixed::<32>(val, "sha256 must be 32 bytes")?),
            _ => return Err(SnapshotError::Invalid("unknown alias row key")),
        }
    }
    Ok(AliasTuple {
        blake3: blake3.ok_or(SnapshotError::Invalid("row missing blake3"))?,
        size: size.ok_or(SnapshotError::Invalid("row missing size"))?,
        crc32: crc32.ok_or(SnapshotError::Invalid("row missing crc32"))?,
        md5: md5.ok_or(SnapshotError::Invalid("row missing md5"))?,
        sha1: sha1.ok_or(SnapshotError::Invalid("row missing sha1"))?,
        sha256: sha256.ok_or(SnapshotError::Invalid("row missing sha256"))?,
    })
}

fn source_to_value(source: &SourceRef) -> Value {
    Value::Map(vec![
        (SRCKEY_PROVIDER, Value::Text(source.provider.clone())),
        (SRCKEY_SYSTEM, Value::Text(source.system.clone())),
        (SRCKEY_DAT_BLOB, Value::Bytes(source.dat_blob.0.to_vec())),
        (SRCKEY_IMPORTED_AT, Value::Uint(source.imported_at)),
    ])
}

fn source_from_value(value: &Value) -> Result<SourceRef, SnapshotError> {
    let (mut provider, mut system, mut dat_blob, mut imported_at) = (None, None, None, None);
    for (key, val) in as_map(value)? {
        match *key {
            SRCKEY_PROVIDER => provider = Some(as_text(val)?.to_owned()),
            SRCKEY_SYSTEM => system = Some(as_text(val)?.to_owned()),
            SRCKEY_DAT_BLOB => dat_blob = Some(as_hash(val)?),
            SRCKEY_IMPORTED_AT => imported_at = Some(as_uint(val)?),
            _ => return Err(SnapshotError::Invalid("unknown source key")),
        }
    }
    Ok(SourceRef {
        provider: provider.ok_or(SnapshotError::Invalid("source missing provider"))?,
        system: system.ok_or(SnapshotError::Invalid("source missing system"))?,
        dat_blob: dat_blob.ok_or(SnapshotError::Invalid("source missing dat blob"))?,
        imported_at: imported_at.ok_or(SnapshotError::Invalid("source missing imported_at"))?,
    })
}

fn expect_object<'a>(
    bytes: &'a [u8],
    kind: ObjectKind,
    version: u32,
    what: &'static str,
) -> Result<&'a [u8], SnapshotError> {
    let (got_kind, got_version, body) =
        object::sniff(bytes).ok_or(SnapshotError::WrongKind(what))?;
    if got_kind != kind {
        return Err(SnapshotError::WrongKind(what));
    }
    if got_version != version {
        return Err(SnapshotError::Version(what, got_version));
    }
    Ok(&bytes[body..])
}

fn as_map(value: &Value) -> Result<&[(u64, Value)], SnapshotError> {
    match value {
        Value::Map(entries) => Ok(entries),
        _ => Err(SnapshotError::Invalid("expected map")),
    }
}

fn as_text(value: &Value) -> Result<&str, SnapshotError> {
    match value {
        Value::Text(t) => Ok(t),
        _ => Err(SnapshotError::Invalid("expected text")),
    }
}

fn as_uint(value: &Value) -> Result<u64, SnapshotError> {
    match value {
        Value::Uint(n) => Ok(*n),
        _ => Err(SnapshotError::Invalid("expected unsigned integer")),
    }
}

fn as_bytes(value: &Value) -> Result<&[u8], SnapshotError> {
    match value {
        Value::Bytes(b) => Ok(b),
        _ => Err(SnapshotError::Invalid("expected byte string")),
    }
}

fn as_fixed<const N: usize>(value: &Value, err: &'static str) -> Result<[u8; N], SnapshotError> {
    as_bytes(value)?
        .try_into()
        .map_err(|_| SnapshotError::Invalid(err))
}

fn as_hash(value: &Value) -> Result<Blake3, SnapshotError> {
    Ok(Blake3(as_fixed::<32>(
        value,
        "hash must be exactly 32 bytes",
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuple(seed: u8) -> AliasTuple {
        AliasTuple {
            size: u64::from(seed) * 1024,
            crc32: [seed; 4],
            md5: [seed; 16],
            sha1: [seed; 20],
            sha256: [seed; 32],
            blake3: Blake3::compute(&[seed]),
        }
    }

    fn golden_payload() -> SnapshotPayload {
        let batch_a = AliasBatch {
            rows: vec![tuple(1), tuple(2)],
        };
        let batch_b = AliasBatch::default();
        SnapshotPayload {
            sequence: 7,
            created_at: 1_751_800_000,
            sources: vec![
                SourceRef {
                    provider: "No-Intro".into(),
                    system: "Nintendo - Game Boy".into(),
                    dat_blob: Blake3::compute(b"a dat file"),
                    imported_at: 1_751_700_000,
                },
                SourceRef {
                    provider: "Redump".into(),
                    system: "Sony - PlayStation".into(),
                    dat_blob: Blake3::compute(b"another dat file"),
                    imported_at: 1_751_700_100,
                },
            ],
            alias_fanout: 2,
            alias_batches: vec![
                Blake3::compute(&batch_a.encode().expect("valid")),
                Blake3::compute(&batch_b.encode().expect("valid")),
            ],
            analysis_fanout: 0,
            analysis_batches: Vec::new(),
            // Empty encodes as absence: the pinned golden hash below
            // proves keys 8/9 were a compatible, additive change.
            tags: Vec::new(),
            config: Vec::new(),
        }
    }

    #[test]
    fn tags_and_config_round_trip() {
        let id = Identity::from_seed([9u8; 32]);
        let mut payload = golden_payload();
        payload.tags = vec![
            ("view/gba".into(), Blake3::compute(b"snap a")),
            ("view/psx".into(), Blake3::compute(b"snap b")),
        ];
        payload.config = vec![
            ("view:gba".into(), vec![1, 2, 3]),
            ("view:psx".into(), vec![4, 5]),
        ];
        let encoded = payload.encode_signed(&id).expect("valid");
        let decoded = StateSnapshot::decode(&encoded).expect("decodes");
        assert_eq!(decoded.payload, payload);

        // canonicality: unsorted or empty-keyed rows are rejected
        let mut unsorted = payload.clone();
        unsorted.tags.swap(0, 1);
        assert!(unsorted.encode_signed(&id).is_err());
        let mut empty_key = payload.clone();
        empty_key.config[0].0 = String::new();
        assert!(empty_key.encode_signed(&id).is_err());
    }

    fn analysis_row(seed: u8, positive: bool) -> AnalysisRow {
        AnalysisRow {
            blob: Blake3::compute(&[seed]),
            analyzer: Blake3::compute(b"datboi-analyzer:noop/1"),
            positive,
            detail: (!positive).then(|| "nothing found".to_owned()),
        }
    }

    /// FORMAT COMMITMENT for both object kinds, like the recipe golden
    /// vector: signing is deterministic (RFC 8032), so a fixed seed over a
    /// fixed payload yields fixed snapshot bytes.
    #[test]
    fn golden_vector_identity() {
        let id = Identity::from_seed([42u8; 32]);
        let encoded = golden_payload().encode_signed(&id).expect("valid");
        assert!(encoded.starts_with(b"datboi/statesnap/1\n"));
        assert_eq!(
            Blake3::compute(&encoded).to_hex(),
            "f7ffe0f6b7a67955780b600a7f8cef5fc72cbfa5099e3e773421371a36ee3efd"
        );

        let batch = AliasBatch {
            rows: vec![tuple(1), tuple(2)],
        };
        let batch_bytes = batch.encode().expect("valid");
        assert!(batch_bytes.starts_with(b"datboi/aliases/1\n"));
        assert_eq!(
            Blake3::compute(&batch_bytes).to_hex(),
            "716e5970588c9642c147bb8ae993db8f89027892edba90403644504ba623d62f"
        );
    }

    /// The golden vector above has NO analysis fields — pre-D48 snapshot
    /// bytes must decode and re-encode unchanged. This test covers the
    /// extended payload and the analysis batch codec.
    #[test]
    fn analysis_batches_round_trip_and_extend_the_payload() {
        let batch = AnalysisBatch {
            rows: vec![analysis_row(2, true), analysis_row(1, false)],
        };
        let bytes = batch.encode().expect("valid");
        assert!(bytes.starts_with(b"datboi/analysis/1\n"));
        let decoded = AnalysisBatch::decode(&bytes).expect("decodes");
        let mut expected = batch.rows.clone();
        expected.sort_by_key(|r| (r.blob, r.analyzer));
        assert_eq!(decoded.rows, expected);

        // Duplicate (blob, analyzer) rejected.
        let dup = AnalysisBatch {
            rows: vec![analysis_row(1, false), analysis_row(1, true)],
        };
        assert_eq!(
            dup.encode(),
            Err(SnapshotError::Invalid(
                "duplicate (blob, analyzer) in analysis batch"
            ))
        );

        let id = Identity::from_seed([42u8; 32]);
        let mut payload = golden_payload();
        payload.analysis_fanout = 1;
        payload.analysis_batches = vec![Blake3::compute(&bytes)];
        let encoded = payload.encode_signed(&id).expect("valid");
        let decoded = StateSnapshot::decode(&encoded).expect("decodes");
        assert_eq!(decoded.payload, payload);

        // Mismatched fanout/batches rejected.
        let mut bad = payload;
        bad.analysis_batches.clear();
        assert_eq!(
            bad.encode_signed(&id),
            Err(SnapshotError::Invalid("analysis batch count != fanout"))
        );
    }

    #[test]
    fn snapshot_round_trips_and_verifies() {
        let id = Identity::from_seed([42u8; 32]);
        let payload = golden_payload();
        let encoded = payload.encode_signed(&id).expect("valid");
        let decoded = StateSnapshot::decode(&encoded).expect("decodes");
        assert_eq!(decoded.payload, payload);
        assert_eq!(decoded.public_key, id.public_key());
        decoded
            .verify(&id.public_key())
            .expect("pinned key matches");

        let other = Identity::from_seed([9u8; 32]);
        assert_eq!(
            decoded.verify(&other.public_key()),
            Err(SnapshotError::BadSignature)
        );
    }

    #[test]
    fn tampered_snapshot_is_rejected() {
        let id = Identity::from_seed([42u8; 32]);
        let mut encoded = golden_payload().encode_signed(&id).expect("valid");
        // Flip one bit inside the CBOR body (past the header).
        let target = encoded.len() - 1;
        encoded[target] ^= 1;
        // Either the CBOR/format layer or the signature check must reject.
        assert!(StateSnapshot::decode(&encoded).is_err());
    }

    #[test]
    fn batch_round_trips_sorted() {
        // Encoder sorts; decoder gets ascending rows back out.
        let batch = AliasBatch {
            rows: vec![tuple(9), tuple(3), tuple(6)],
        };
        let decoded = AliasBatch::decode(&batch.encode().expect("valid")).expect("decodes");
        let mut expected = batch.rows;
        expected.sort_by_key(|r| r.blake3);
        assert_eq!(decoded.rows, expected);
    }

    #[test]
    fn batch_rejects_duplicates() {
        let batch = AliasBatch {
            rows: vec![tuple(3), tuple(3)],
        };
        assert_eq!(
            batch.encode(),
            Err(SnapshotError::Invalid("duplicate blake3 in alias batch"))
        );
    }

    #[test]
    fn rejects_structural_violations() {
        let id = Identity::from_seed([42u8; 32]);

        let mut wrong_fanout = golden_payload();
        wrong_fanout.alias_fanout = 3; // 3 shards claimed, 2 batches listed
        assert_eq!(
            wrong_fanout.encode_signed(&id),
            Err(SnapshotError::Invalid("alias batch count != fanout"))
        );

        let mut unsorted = golden_payload();
        unsorted.sources.swap(0, 1);
        assert_eq!(
            unsorted.encode_signed(&id),
            Err(SnapshotError::Invalid(
                "sources not sorted by provider/system"
            ))
        );

        assert_eq!(
            StateSnapshot::decode(b"datboi/recipe/1\n\xa0"),
            Err(SnapshotError::WrongKind("statesnap"))
        );
        assert_eq!(
            StateSnapshot::decode(b"datboi/statesnap/2\n\xa0"),
            Err(SnapshotError::Version("statesnap", 2))
        );
    }

    #[test]
    fn shard_assignment_is_total_and_monotone() {
        for fanout in [1, 2, 16, 256] {
            let mut last = 0;
            for byte in 0..=255u8 {
                let mut h = [0u8; 32];
                h[0] = byte;
                let shard = alias_shard(&Blake3(h), fanout);
                assert!(shard < fanout);
                assert!(shard >= last, "assignment must be monotone");
                last = shard;
            }
            assert_eq!(last, fanout - 1, "top shard must be reachable");
        }
    }
}
