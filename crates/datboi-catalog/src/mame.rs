//! MAME merge-mode rendering (the D31 deferred set, 60-dats.md §6):
//! merge modes are output transforms, not storage — the schema stores
//! flat per-machine claims (romof/cloneof surrogates, per-rom
//! merge_name, is_device/is_bios flags, `@device_refs` in attrs) and
//! this module renders any layout from them at view-eval time.
//!
//! Set semantics (the clrmamepro/RomVault conventions):
//! - **non-merged**: every non-device machine is a standalone set —
//!   its full listed claim set (listxml already lists inherited
//!   parent/bios roms per machine, merge-tagged) plus the transitive
//!   `device_ref` closure's roms. Device machines emit no set.
//! - **split**: every machine (including bios and device machines)
//!   emits only its OWN roms — claims carrying a merge name live in
//!   the parent set and are excluded here.
//! - **merged**: clones fold into their cloneof parent's set (parent
//!   claims + every clone's non-merge claims, (name, hash)-deduped);
//!   bios and device machines stay separate sets, exactly as in split.
//!
//! Disk (CHD) claims render with their conventional `.chd` extension
//! in every mode. Dangling `device_ref`s (a name with no entry in the
//! revision) are counted, not fatal — real listxml has them.

use std::collections::{HashMap, HashSet};

use datboi_core::hash::Blake3;
use datboi_index::{ClaimKind, Db};
use rusqlite::params;

use crate::CatalogError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MameMode {
    NonMerged,
    Split,
    Merged,
}

impl MameMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NonMerged => "non-merged",
            Self::Split => "split",
            Self::Merged => "merged",
        }
    }

    /// CBOR / CLI codes (ViewDef key 12).
    #[must_use]
    pub fn code(self) -> u64 {
        match self {
            Self::NonMerged => 1,
            Self::Split => 2,
            Self::Merged => 3,
        }
    }

    #[must_use]
    pub fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::NonMerged),
            2 => Some(Self::Split),
            3 => Some(Self::Merged),
            _ => None,
        }
    }

    /// # Errors
    /// Unknown mode name.
    pub fn parse(name: &str) -> Result<Self, CatalogError> {
        match name {
            "non-merged" | "nonmerged" | "full" => Ok(Self::NonMerged),
            "split" => Ok(Self::Split),
            "merged" => Ok(Self::Merged),
            other => Err(CatalogError::Mame(format!(
                "unknown mame mode {other:?} (non-merged, split, merged)"
            ))),
        }
    }
}

/// One renderable file: which set directory it lives in, its file
/// name, and the held blob backing it.
#[derive(Debug, Clone)]
pub struct SetRow {
    /// The set's anchor entry (parent for merged clones, the device
    /// entry for device sets) — what selection/reporting keys on.
    pub entry_id: i64,
    /// Set directory name (`{entry}` in the template).
    pub set_name: String,
    /// File name within the set (`{name}` in the template).
    pub file_name: String,
    pub hash: Blake3,
    pub size: u64,
}

#[derive(Debug, Default)]
pub struct MameRender {
    pub rows: Vec<SetRow>,
    /// `device_ref` names with no entry in the revision.
    pub dangling_device_refs: usize,
}

struct EntryInfo {
    name: String,
    is_device: bool,
    is_bios: bool,
    cloneof_id: Option<i64>,
    device_refs: Vec<String>,
}

#[derive(Clone)]
struct HeldClaim {
    name: String,
    merge_name: Option<String>,
    kind: ClaimKind,
    hash: Blake3,
    size: u64,
}

/// Render the revision's held-and-verified claims as merge-mode sets.
///
/// # Errors
/// Index I/O; corrupt attrs.
pub fn render_sets(db: &Db, revision_id: i64, mode: MameMode) -> Result<MameRender, CatalogError> {
    // Entry graph, including the `@device_refs` attrs array.
    let mut entries: HashMap<i64, EntryInfo> = HashMap::new();
    let mut id_by_name: HashMap<String, i64> = HashMap::new();
    {
        let conn = db.cache();
        let mut stmt = conn.prepare(
            "SELECT entry_id, name, is_device, is_bios, cloneof_id, json(attrs)
             FROM entry WHERE revision_id = ?1",
        )?;
        let rows = stmt.query_map(params![revision_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        for row in rows {
            let (entry_id, name, is_device, is_bios, cloneof_id, attrs) = row?;
            let device_refs = attrs
                .as_deref()
                .map(parse_device_refs)
                .transpose()?
                .unwrap_or_default();
            id_by_name.insert(name.clone(), entry_id);
            entries.insert(
                entry_id,
                EntryInfo {
                    name,
                    is_device,
                    is_bios,
                    cloneof_id,
                    device_refs,
                },
            );
        }
    }

    // Held-and-verified claims per entry (the same grounding joins the
    // plain view path uses).
    let mut held: HashMap<i64, Vec<HeldClaim>> = HashMap::new();
    {
        let conn = db.cache();
        let mut stmt = conn.prepare(
            "SELECT e.entry_id, rc.name, rc.merge_name, rc.kind,
                    MIN(b.hash), MAX(b.size)
             FROM entry e
             JOIN rom_claim rc ON rc.entry_id = e.entry_id
             JOIN identity_status s ON s.identity_id = rc.identity_id AND s.state = 4
             JOIN identity_blob ib ON ib.identity_id = rc.identity_id AND ib.basis >= 1
             JOIN blob b ON b.blob_id = ib.blob_id
             WHERE e.revision_id = ?1 AND rc.status != 2 AND NOT rc.optional
             GROUP BY rc.claim_id
             ORDER BY e.name, rc.name",
        )?;
        let rows = stmt.query_map(params![revision_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, [u8; 32]>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })?;
        for row in rows {
            let (entry_id, name, merge_name, kind, hash, size) = row?;
            held.entry(entry_id).or_default().push(HeldClaim {
                name,
                merge_name,
                kind: ClaimKind::from_code(kind)?,
                hash: Blake3(hash),
                size: u64::try_from(size.unwrap_or(0)).unwrap_or(0),
            });
        }
    }

    // Deterministic set iteration: by entry name.
    let mut ordered: Vec<(&i64, &EntryInfo)> = entries.iter().collect();
    ordered.sort_by(|a, b| a.1.name.cmp(&b.1.name));

    let mut render = MameRender::default();
    let empty: Vec<HeldClaim> = Vec::new();
    for (&entry_id, info) in ordered {
        let own = held.get(&entry_id).unwrap_or(&empty);
        // Which set does this entry's output belong to, and which
        // claims ride along?
        let mut set: Vec<HeldClaim> = Vec::new();
        let (anchor, set_name) = match mode {
            MameMode::NonMerged => {
                if info.is_device {
                    continue; // device roms fold into referencing machines
                }
                set.extend(own.iter().cloned());
                let dangling = closure_claims(&entries, &id_by_name, &held, info, &mut set);
                render.dangling_device_refs += dangling;
                (entry_id, info.name.clone())
            }
            MameMode::Split => {
                set.extend(own.iter().filter(|c| c.merge_name.is_none()).cloned());
                (entry_id, info.name.clone())
            }
            MameMode::Merged => {
                if info.is_device || info.is_bios {
                    set.extend(own.iter().filter(|c| c.merge_name.is_none()).cloned());
                    (entry_id, info.name.clone())
                } else {
                    // Fold into the top of the cloneof chain.
                    let mut top_id = entry_id;
                    let mut hops = 0;
                    while let Some(parent) = entries.get(&top_id).and_then(|e| e.cloneof_id) {
                        top_id = parent;
                        hops += 1;
                        if hops > 32 {
                            return Err(CatalogError::Mame(format!(
                                "cloneof chain too deep at {}",
                                info.name
                            )));
                        }
                    }
                    let top_name = entries
                        .get(&top_id)
                        .map_or_else(|| info.name.clone(), |e| e.name.clone());
                    set.extend(own.iter().filter(|c| c.merge_name.is_none()).cloned());
                    (top_id, top_name)
                }
            }
        };
        emit(&mut render.rows, anchor, &set_name, &set);
    }
    // Merged mode emits the same set name from several entries;
    // (set, file, hash)-level dedup happens in emit's caller order, so
    // finish with a global (path-shaped) dedup keeping the first.
    let mut seen: HashSet<(String, String, Blake3)> = HashSet::new();
    render
        .rows
        .retain(|r| seen.insert((r.set_name.clone(), r.file_name.clone(), r.hash)));
    Ok(render)
}

/// Transitive `device_ref` closure: append every referenced device's
/// claims (and their refs') to `set`. Returns the dangling-ref count.
fn closure_claims(
    entries: &HashMap<i64, EntryInfo>,
    id_by_name: &HashMap<String, i64>,
    held: &HashMap<i64, Vec<HeldClaim>>,
    start: &EntryInfo,
    set: &mut Vec<HeldClaim>,
) -> usize {
    let mut dangling = 0;
    let mut visited: HashSet<i64> = HashSet::new();
    let mut queue: Vec<&str> = start.device_refs.iter().map(String::as_str).collect();
    while let Some(name) = queue.pop() {
        let Some(&id) = id_by_name.get(name) else {
            dangling += 1;
            continue;
        };
        if !visited.insert(id) {
            continue;
        }
        if let Some(claims) = held.get(&id) {
            set.extend(claims.iter().cloned());
        }
        if let Some(info) = entries.get(&id) {
            queue.extend(info.device_refs.iter().map(String::as_str));
        }
    }
    dangling
}

fn emit(rows: &mut Vec<SetRow>, anchor: i64, set_name: &str, claims: &[HeldClaim]) {
    let mut seen: HashSet<(String, Blake3)> = HashSet::new();
    for c in claims {
        let file_name = match c.kind {
            // CHDs are named without their extension in dats; the file
            // on disk is `<name>.chd` in every MAME layout.
            ClaimKind::Disk if !c.name.to_ascii_lowercase().ends_with(".chd") => {
                format!("{}.chd", c.name)
            }
            _ => c.name.clone(),
        };
        if !seen.insert((file_name.clone(), c.hash)) {
            continue; // identical (name, content) listed twice: one file
        }
        rows.push(SetRow {
            entry_id: anchor,
            set_name: set_name.to_owned(),
            file_name,
            hash: c.hash,
            size: c.size,
        });
    }
}

/// Pull `@device_refs` back out of the entry attrs JSON.
pub(crate) fn parse_device_refs(attrs_json: &str) -> Result<Vec<String>, CatalogError> {
    let value: serde_json::Value =
        serde_json::from_str(attrs_json).map_err(|_| CatalogError::Corrupt("entry attrs"))?;
    let Some(refs) = value.get("@device_refs") else {
        return Ok(Vec::new());
    };
    refs.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .ok_or(CatalogError::Corrupt("entry attrs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_names_round_trip() {
        for mode in [MameMode::NonMerged, MameMode::Split, MameMode::Merged] {
            assert_eq!(MameMode::parse(mode.as_str()).unwrap(), mode);
            assert_eq!(MameMode::from_code(mode.code()), Some(mode));
        }
        assert!(MameMode::parse("zipped").is_err());
        assert_eq!(MameMode::from_code(0), None);
    }

    #[test]
    fn disks_gain_chd_and_duplicates_collapse() {
        let claim = |name: &str, kind: ClaimKind, seed: u8| HeldClaim {
            name: name.to_owned(),
            merge_name: None,
            kind,
            hash: Blake3::compute(&[seed]),
            size: 1,
        };
        let mut rows = Vec::new();
        emit(
            &mut rows,
            1,
            "set",
            &[
                claim("game disc", ClaimKind::Disk, 1),
                claim("already.chd", ClaimKind::Disk, 2),
                claim("a.rom", ClaimKind::Rom, 3),
                claim("a.rom", ClaimKind::Rom, 3), // identical duplicate
            ],
        );
        let names: Vec<&str> = rows.iter().map(|r| r.file_name.as_str()).collect();
        assert_eq!(names, vec!["game disc.chd", "already.chd", "a.rom"]);
    }

    #[test]
    fn device_refs_parse_from_attrs() {
        assert_eq!(
            parse_device_refs(r#"{"@device_refs":["z80","ym2610"],"other":1}"#).unwrap(),
            vec!["z80", "ym2610"]
        );
        assert!(parse_device_refs(r#"{"noRefs":true}"#).unwrap().is_empty());
        assert!(parse_device_refs("not json").is_err());
    }
}
