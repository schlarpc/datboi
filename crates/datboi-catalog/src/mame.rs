//! MAME merge-mode rendering (the D31 deferred set, dats.md §6):
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

/// Which machines a MAME view offers as sets of its own (D130).
///
/// Curation is a question about the *menu*, not about storage. Every
/// filter here decides whether an entry emits a set directory; none of
/// them decide whether an entry's roms reach the sets that reference
/// it. A dropped BIOS machine's roms still ride inside every non-merged
/// game that inherits them, and a dropped device's roms still arrive
/// through the `device_ref` closure — which is why the filters are
/// applied at the emit decision below and never to the SQL that loads
/// the entry graph.
///
/// Default is everything false: an undefined curation renders exactly
/// what it rendered before this existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MameCuration {
    /// Drop what MAME cannot run: `runnable="no"`, or a driver whose
    /// overall `status` is `preliminary`. Keeps `good` and `imperfect`
    /// — imperfect is "the sound is wrong", not "it does not boot", and
    /// a cabinet would rather show it than hide it.
    pub working_only: bool,
    /// Drop clones, leaving one set per parent machine.
    pub parents_only: bool,
    /// Drop BIOS machines as sets of their own.
    pub no_bios: bool,
    /// Drop mechanical machines: pinball, slots, redemption cabinets —
    /// things with no video for a monitor to show.
    pub no_mechanical: bool,
}

impl MameCuration {
    /// CBOR / CLI bitmask (ViewDef key 13). Additive: a new filter
    /// takes the next free bit and old definitions decode with it off.
    const BIT_WORKING: u64 = 1;
    const BIT_PARENTS: u64 = 2;
    const BIT_NO_BIOS: u64 = 4;
    const BIT_NO_MECHANICAL: u64 = 8;

    #[must_use]
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }

    #[must_use]
    pub fn bits(self) -> u64 {
        let mut bits = 0;
        if self.working_only {
            bits |= Self::BIT_WORKING;
        }
        if self.parents_only {
            bits |= Self::BIT_PARENTS;
        }
        if self.no_bios {
            bits |= Self::BIT_NO_BIOS;
        }
        if self.no_mechanical {
            bits |= Self::BIT_NO_MECHANICAL;
        }
        bits
    }

    /// Unknown bits are ignored rather than rejected: a definition
    /// written by a newer datboi still decodes here, minus the filters
    /// this build does not have. The alternative — refusing to read the
    /// view at all — turns a forward-compatible policy into an outage.
    #[must_use]
    pub fn from_bits(bits: u64) -> Self {
        Self {
            working_only: bits & Self::BIT_WORKING != 0,
            parents_only: bits & Self::BIT_PARENTS != 0,
            no_bios: bits & Self::BIT_NO_BIOS != 0,
            no_mechanical: bits & Self::BIT_NO_MECHANICAL != 0,
        }
    }

    /// Parse the CLI's comma-separated filter names.
    ///
    /// # Errors
    /// Unknown filter name.
    pub fn parse_list<S: AsRef<str>>(names: &[S]) -> Result<Self, CatalogError> {
        let mut out = Self::default();
        for name in names {
            match name.as_ref() {
                "working" => out.working_only = true,
                "parents-only" | "parents" | "no-clones" => out.parents_only = true,
                "no-bios" => out.no_bios = true,
                "no-mechanical" => out.no_mechanical = true,
                other => {
                    return Err(CatalogError::Mame(format!(
                        "unknown mame curation filter {other:?} \
                         (working, parents-only, no-bios, no-mechanical)"
                    )));
                }
            }
        }
        Ok(out)
    }

    /// Human-readable filter list, for `view define` / `view list`.
    #[must_use]
    pub fn describe(self) -> String {
        if self.is_empty() {
            return "none".to_owned();
        }
        self.names().join("+")
    }

    /// The filters this curation has on, in a stable order.
    #[must_use]
    pub fn names(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.working_only {
            out.push("working");
        }
        if self.parents_only {
            out.push("parents-only");
        }
        if self.no_bios {
            out.push("no-bios");
        }
        if self.no_mechanical {
            out.push("no-mechanical");
        }
        out
    }

    /// Does this entry get a set directory of its own?
    fn keeps(self, info: &EntryInfo) -> bool {
        if self.working_only && (!info.runnable || info.driver_preliminary) {
            return false;
        }
        if self.parents_only && info.cloneof_id.is_some() {
            return false;
        }
        if self.no_bios && info.is_bios {
            return false;
        }
        if self.no_mechanical && info.is_mechanical {
            return false;
        }
        true
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
    is_mechanical: bool,
    runnable: bool,
    /// `driver:status` from the listxml was `preliminary` — the driver
    /// is a stub. Stored as the one bit anything asks about rather than
    /// the string, since every other status is "it runs".
    driver_preliminary: bool,
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

/// Render the revision's held-and-verified claims as merge-mode sets,
/// keeping only the machines `curation` offers as sets (D130).
///
/// # Errors
/// Index I/O; corrupt attrs.
pub fn render_sets(
    db: &Db,
    revision_id: i64,
    mode: MameMode,
    curation: MameCuration,
) -> Result<MameRender, CatalogError> {
    // Entry graph, including the `@device_refs` attrs array.
    let mut entries: HashMap<i64, EntryInfo> = HashMap::new();
    let mut id_by_name: HashMap<String, i64> = HashMap::new();
    {
        let conn = db.cache();
        // Every machine is loaded, curated or not: a filtered-out BIOS
        // or device still has to be reachable through `device_ref` and
        // the cloneof chain. Curation is applied at the emit decision.
        let mut stmt = conn.prepare(
            "SELECT entry_id, name, is_device, is_bios, is_mechanical, runnable,
                    cloneof_id, json(attrs)
             FROM entry WHERE revision_id = ?1",
        )?;
        let rows = stmt.query_map(params![revision_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        for row in rows {
            let (entry_id, name, is_device, is_bios, is_mechanical, runnable, cloneof_id, attrs) =
                row?;
            let device_refs = attrs
                .as_deref()
                .map(parse_device_refs)
                .transpose()?
                .unwrap_or_default();
            let driver_preliminary = attrs
                .as_deref()
                .map(parse_driver_preliminary)
                .transpose()?
                .unwrap_or(false);
            id_by_name.insert(name.clone(), entry_id);
            entries.insert(
                entry_id,
                EntryInfo {
                    name,
                    is_device,
                    is_bios,
                    is_mechanical,
                    runnable,
                    driver_preliminary,
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
        if !curation.keeps(info) {
            continue;
        }
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

/// Is the listxml's `<driver status="...">` `preliminary`?
///
/// The importer flattens child elements as `elem:name`, so this is the
/// `driver:status` key. Absent means the machine has no driver element
/// at all (BIOS and device entries), which is not a claim that it is
/// broken — those are excluded by their own flags if at all.
pub(crate) fn parse_driver_preliminary(attrs_json: &str) -> Result<bool, CatalogError> {
    let value: serde_json::Value =
        serde_json::from_str(attrs_json).map_err(|_| CatalogError::Corrupt("entry attrs"))?;
    Ok(value
        .get("driver:status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("preliminary")))
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

    fn info(name: &str) -> EntryInfo {
        EntryInfo {
            name: name.to_owned(),
            is_device: false,
            is_bios: false,
            is_mechanical: false,
            runnable: true,
            driver_preliminary: false,
            cloneof_id: None,
            device_refs: Vec::new(),
        }
    }

    #[test]
    fn curation_bits_round_trip_and_ignore_unknown() {
        let all = MameCuration {
            working_only: true,
            parents_only: true,
            no_bios: true,
            no_mechanical: true,
        };
        assert_eq!(MameCuration::from_bits(all.bits()), all);
        assert!(MameCuration::default().is_empty());
        assert_eq!(MameCuration::from_bits(0), MameCuration::default());
        // A filter a newer datboi wrote decodes as the ones we know,
        // never as an error: an unreadable policy would be an outage.
        assert_eq!(MameCuration::from_bits(all.bits() | 1 << 40), all);
    }

    #[test]
    fn curation_parses_its_cli_names() {
        assert_eq!(
            MameCuration::parse_list(&["working", "no-clones"]).unwrap(),
            MameCuration {
                working_only: true,
                parents_only: true,
                ..MameCuration::default()
            }
        );
        assert_eq!(
            MameCuration::parse_list(&["working", "parents-only", "no-bios", "no-mechanical"])
                .unwrap()
                .describe(),
            "working+parents-only+no-bios+no-mechanical"
        );
        assert_eq!(MameCuration::default().describe(), "none");
        assert!(MameCuration::parse_list(&["no-vector"]).is_err());
    }

    #[test]
    fn curation_keeps_what_each_filter_names() {
        let empty = MameCuration::default();
        let working = MameCuration {
            working_only: true,
            ..empty
        };
        let mut unrunnable = info("unrunnable");
        unrunnable.runnable = false;
        let mut prelim = info("prelim");
        prelim.driver_preliminary = true;
        let mut clone = info("clone");
        clone.cloneof_id = Some(7);
        let mut bios = info("neogeo");
        bios.is_bios = true;
        let mut mech = info("pinball");
        mech.is_mechanical = true;

        // Nothing on means nothing is dropped: a definition written
        // before curation existed renders exactly as it did.
        for entry in [&unrunnable, &prelim, &clone, &bios, &mech] {
            assert!(empty.keeps(entry), "{} dropped by no filters", entry.name);
        }

        assert!(working.keeps(&info("pacman")));
        assert!(!working.keeps(&unrunnable));
        assert!(!working.keeps(&prelim));
        // `working` says nothing about lineage or hardware class.
        assert!(working.keeps(&clone));
        assert!(working.keeps(&bios));

        assert!(
            !MameCuration {
                parents_only: true,
                ..empty
            }
            .keeps(&clone)
        );
        assert!(
            !MameCuration {
                no_bios: true,
                ..empty
            }
            .keeps(&bios)
        );
        assert!(
            !MameCuration {
                no_mechanical: true,
                ..empty
            }
            .keeps(&mech)
        );
    }

    #[test]
    fn driver_status_reads_preliminary_only() {
        assert!(parse_driver_preliminary(r#"{"driver:status":"preliminary"}"#).unwrap());
        assert!(parse_driver_preliminary(r#"{"driver:status":"Preliminary"}"#).unwrap());
        for status in ["good", "imperfect"] {
            let json = format!(r#"{{"driver:status":"{status}"}}"#);
            assert!(!parse_driver_preliminary(&json).unwrap());
        }
        // No driver element at all (BIOS/device entries) is not a claim
        // that the machine is broken.
        assert!(!parse_driver_preliminary("{}").unwrap());
        assert!(parse_driver_preliminary("not json").is_err());
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
