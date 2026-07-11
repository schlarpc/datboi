//! Retool clonelists (D57): an additive family/region input for 1G1R.
//! A clonelist is community-curated JSON grouping titles the dat's
//! clone graph misses (cross-region renames, compilation variants).
//! We consume the two non-regex `nameType`s (`short`, `full`); regex
//! entries are counted and skipped — fidelity grows when a corpus
//! demands it.
//!
//! Custody follows D16's acquisition pattern (manual drop now,
//! auto-fetch later): the raw JSON becomes a content-addressed meta
//! blob; the config KV points `clonelist:<provider>/<system>` at its
//! hash (riding the state snapshot); evaluation loads and parses it
//! per eval — no derived rows to drift.

use std::collections::HashMap;
use std::io::Read as _;

use datboi_core::hash::Blake3;
use datboi_index::Db;
use datboi_store_fs::{Namespace as StoreNs, Store};

use crate::CatalogError;
use crate::selection::Clonelist;

fn config_key(provider: &str, system: &str) -> String {
    format!("clonelist:{provider}/{system}")
}

#[derive(Debug)]
pub struct ClonelistReport {
    /// The stored blob.
    pub hash: Blake3,
    /// searchTerm → group mappings usable by selection.
    pub terms: usize,
    /// Entries skipped (regex nameType, malformed rows).
    pub skipped: usize,
}

/// Import a retool clonelist for one dat source: store the raw JSON
/// (meta namespace), validate it parses, point the config KV at it.
///
/// # Errors
/// Unparseable JSON is refused (nothing is linked); I/O as usual.
pub fn import_clonelist(
    db: &Db,
    store: &Store,
    provider: &str,
    system: &str,
    json: &[u8],
) -> Result<ClonelistReport, CatalogError> {
    let (map, skipped) = parse_clonelist(json)?;
    let hash = Blake3::compute(json);
    store.put(StoreNs::Meta, hash, json)?;
    db.upsert_blob(
        &hash,
        Some(json.len() as u64),
        datboi_index::Namespace::Meta,
        datboi_index::Residency::Resident,
    )?;
    db.config_set(&config_key(provider, system), hash.to_hex().as_bytes())?;
    Ok(ClonelistReport {
        hash,
        terms: map.len(),
        skipped,
    })
}

/// Load the source's clonelist, if one is linked. A linked-but-missing
/// or unparseable blob is an error (the operator pinned it; silently
/// selecting without it would change picks behind their back).
///
/// # Errors
/// Index/store I/O; corrupt pointer or blob.
pub fn load_clonelist(
    db: &Db,
    store: &Store,
    provider: &str,
    system: &str,
) -> Result<Option<Clonelist>, CatalogError> {
    let Some(pointer) = db.config_get(&config_key(provider, system))? else {
        return Ok(None);
    };
    let hex = std::str::from_utf8(&pointer).map_err(|_| CatalogError::Corrupt("clonelist"))?;
    let hash: Blake3 = hex
        .parse()
        .map_err(|_| CatalogError::Corrupt("clonelist"))?;
    let mut bytes = Vec::new();
    store
        .get(StoreNs::Meta, &hash)?
        .ok_or(CatalogError::Corrupt("clonelist"))?
        .read_to_end(&mut bytes)?;
    let (map, _) = parse_clonelist(&bytes)?;
    Ok(Some(map))
}

/// Parse retool's clonelist JSON: `variants[].group` +
/// `variants[].titles[].searchTerm` (nameType short/full; both are
/// case-folded lookups on our side). Returns (map, skipped).
fn parse_clonelist(json: &[u8]) -> Result<(Clonelist, usize), CatalogError> {
    let value: serde_json::Value = serde_json::from_slice(json)
        .map_err(|e| CatalogError::Clonelist(format!("does not parse: {e}")))?;
    let Some(variants) = value.get("variants").and_then(|v| v.as_array()) else {
        return Err(CatalogError::Corrupt("clonelist"));
    };
    let mut map: Clonelist = HashMap::new();
    let mut skipped = 0usize;
    for variant in variants {
        let Some(group) = variant.get("group").and_then(|g| g.as_str()) else {
            skipped += 1;
            continue;
        };
        let Some(titles) = variant.get("titles").and_then(|t| t.as_array()) else {
            skipped += 1;
            continue;
        };
        for title in titles {
            let Some(term) = title.get("searchTerm").and_then(|s| s.as_str()) else {
                skipped += 1;
                continue;
            };
            match title.get("nameType").and_then(|n| n.as_str()) {
                None | Some("short" | "full") => {
                    map.insert(term.to_lowercase(), group.to_owned());
                }
                Some(_) => skipped += 1, // regex and friends: not yet
            }
        }
    }
    Ok((map, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_retool_shape() {
        let json = br#"{
            "description": {"name": "Test"},
            "variants": [
                {"group": "Game, The", "titles": [
                    {"searchTerm": "Game, The"},
                    {"searchTerm": "Gamu za Best", "nameType": "short"},
                    {"searchTerm": "Game.*", "nameType": "regex"}
                ]},
                {"group": "Other", "titles": [
                    {"searchTerm": "Other Game (USA)", "nameType": "full"}
                ]}
            ]
        }"#;
        let (map, skipped) = parse_clonelist(json).expect("parses");
        assert_eq!(map.get("game, the").map(String::as_str), Some("Game, The"));
        assert_eq!(
            map.get("gamu za best").map(String::as_str),
            Some("Game, The")
        );
        assert_eq!(
            map.get("other game (usa)").map(String::as_str),
            Some("Other")
        );
        assert_eq!(skipped, 1, "regex entry skipped");
        assert!(parse_clonelist(b"{}").is_err(), "no variants: refused");
        assert!(parse_clonelist(b"not json").is_err());
    }
}
