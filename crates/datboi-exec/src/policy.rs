//! Molten GC policy (D72/D73): watermarks, orphan grace, keep-marks.
//! Everything here is state.db config KV — authoritative, snapshot-
//! carried, shared verbatim by the daemon's maintenance worker and the
//! CLI (`datboi evict` / `datboi gc`). Parsing lives here so the two
//! surfaces cannot drift.

use std::collections::HashSet;

use datboi_core::hash::Blake3;
use datboi_index::{Db, IndexError};

pub const KEY_HIGH_WATER: &str = "evict:high-water";
pub const KEY_LOW_WATER: &str = "evict:low-water";
pub const KEY_GRACE_SECS: &str = "gc:grace-secs";
/// D91: the affine piece-swap phase (on/off).
pub const KEY_SWAP_ENABLED: &str = "swap:enabled";
/// D91: minimum percentage of a rebuild's input bytes that must be
/// shared (claimed by ≥2 decompositions) or already resident before
/// the swap pays — the never-eager gate.
pub const KEY_SWAP_SHARE_PCT: &str = "swap:share-min-pct";
/// D91/D59 pack-per-chunking: consolidate a chunk set's loose pieces
/// into one sealed pack (on/off).
pub const KEY_CHUNK_PACK_ENABLED: &str = "chunk:pack";
/// Fewest loose pieces a set must have before packing pays — packing a
/// single piece just swaps one inode for another.
pub const KEY_CHUNK_PACK_MIN: &str = "chunk:pack-min-members";
/// Keep-marks: `gc:keep:<hex>` → optional operator note. By HASH, not
/// blob id — intent must survive a cache rebuild (D73).
pub const KEEP_PREFIX: &str = "gc:keep:";

/// D72: armed by default at a safe margin.
pub const DEFAULT_HIGH_PCT: u8 = 90;
pub const DEFAULT_LOW_PCT: u8 = 85;
/// D73 review-eligibility grace from first-observed-unreferenced.
pub const DEFAULT_GRACE_SECS: i64 = 24 * 60 * 60;
/// D91: a lone decomposition (0% sharing) never trips this; a variant
/// pair (MKDS-shaped, ~98% shared) always does.
pub const DEFAULT_SWAP_SHARE_PCT: u8 = 50;
/// Pack-per-chunking: below this many loose pieces the inode saving
/// (N files → 1 pack) doesn't clear the rewrite cost. A CDC set of a
/// ≥4 MiB literal (D59) is dozens of chunks, well past this.
pub const DEFAULT_CHUNK_PACK_MIN: usize = 4;

/// A watermark setting: `off`, a percentage of the store filesystem
/// (`"90%"`), or absolute used bytes (`"500000000000"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watermark {
    Off,
    Pct(u8),
    Bytes(u64),
}

impl Watermark {
    /// The used-bytes threshold this watermark denotes on a filesystem
    /// of `total` bytes; `None` = disarmed.
    #[must_use]
    pub fn threshold_bytes(self, total: u64) -> Option<u64> {
        match self {
            Self::Off => None,
            Self::Pct(pct) => Some(total / 100 * u64::from(pct.min(100))),
            Self::Bytes(bytes) => Some(bytes),
        }
    }

    fn parse(raw: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(raw).ok()?.trim();
        if text.eq_ignore_ascii_case("off") {
            return Some(Self::Off);
        }
        if let Some(pct) = text.strip_suffix('%') {
            return pct.parse::<u8>().ok().filter(|p| *p <= 100).map(Self::Pct);
        }
        text.parse::<u64>().ok().map(Self::Bytes)
    }
}

/// # Errors
/// Index I/O. An unparsable stored value falls back to the default —
/// a typo in policy must not disarm (or arm) eviction silently; the
/// CLI validates on write, this is the belt.
pub fn high_water(db: &Db) -> Result<Watermark, IndexError> {
    Ok(db
        .config_get(KEY_HIGH_WATER)?
        .and_then(|v| Watermark::parse(&v))
        .unwrap_or(Watermark::Pct(DEFAULT_HIGH_PCT)))
}

/// # Errors
/// Index I/O.
pub fn low_water(db: &Db) -> Result<Watermark, IndexError> {
    Ok(db
        .config_get(KEY_LOW_WATER)?
        .and_then(|v| Watermark::parse(&v))
        .unwrap_or(Watermark::Pct(DEFAULT_LOW_PCT)))
}

/// # Errors
/// Index I/O.
pub fn grace_secs(db: &Db) -> Result<i64, IndexError> {
    Ok(db
        .config_get(KEY_GRACE_SECS)?
        .and_then(|v| std::str::from_utf8(&v).ok()?.trim().parse().ok())
        .unwrap_or(DEFAULT_GRACE_SECS))
}

/// D91 swap phase armed? Ambient like the watermarks: the predicate,
/// not the switch, is what keeps it from being eager.
///
/// # Errors
/// Index I/O.
pub fn swap_enabled(db: &Db) -> Result<bool, IndexError> {
    Ok(db
        .config_get(KEY_SWAP_ENABLED)?
        .is_none_or(|v| v != b"0" && !v.eq_ignore_ascii_case(b"off")))
}

/// D91 sharing threshold (percent). Unparsable falls back to the
/// default — a typo must fail toward the bounded posture.
///
/// # Errors
/// Index I/O.
pub fn swap_share_min_pct(db: &Db) -> Result<u8, IndexError> {
    Ok(db
        .config_get(KEY_SWAP_SHARE_PCT)?
        .and_then(|v| std::str::from_utf8(&v).ok()?.trim().parse().ok())
        .filter(|p| *p <= 100)
        .unwrap_or(DEFAULT_SWAP_SHARE_PCT))
}

/// Pack-per-chunking armed? On by default; only fires where a chunk
/// flood exists (route-less ≥4 MiB literals, D59), so it is dormant
/// until chunking has actually run.
///
/// # Errors
/// Index I/O.
pub fn chunk_pack_enabled(db: &Db) -> Result<bool, IndexError> {
    Ok(db
        .config_get(KEY_CHUNK_PACK_ENABLED)?
        .is_none_or(|v| v != b"0" && !v.eq_ignore_ascii_case(b"off")))
}

/// Minimum loose pieces before a set is worth packing (unparsable →
/// default, same bounded-fallback posture as the swap threshold).
///
/// # Errors
/// Index I/O.
pub fn chunk_pack_min_members(db: &Db) -> Result<usize, IndexError> {
    Ok(db
        .config_get(KEY_CHUNK_PACK_MIN)?
        .and_then(|v| std::str::from_utf8(&v).ok()?.trim().parse().ok())
        .filter(|n| *n >= 2)
        .unwrap_or(DEFAULT_CHUNK_PACK_MIN))
}

/// Every kept hash (operator "this is not junk" marks).
///
/// # Errors
/// Index I/O.
pub fn keep_set(db: &Db) -> Result<HashSet<Blake3>, IndexError> {
    Ok(db
        .config_list_prefix(KEEP_PREFIX)?
        .into_iter()
        .filter_map(|(key, _)| key.strip_prefix(KEEP_PREFIX)?.parse().ok())
        .collect())
}

/// Set or clear one keep-mark.
///
/// # Errors
/// Index I/O.
pub fn set_keep(db: &Db, hash: &Blake3, keep: bool) -> Result<(), IndexError> {
    let key = format!("{KEEP_PREFIX}{}", hash.to_hex());
    if keep {
        db.config_set(&key, b"1")
    } else {
        // Empty value = cleared (config_get of empty is still Some, so
        // delete outright).
        db.state()
            .execute("DELETE FROM config WHERE key = ?1", [key])
            .map(|_| ())
            .map_err(Into::into)
    }
}
