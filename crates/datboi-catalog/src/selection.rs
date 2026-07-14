//! 1G1R selection (views.md; retool's filter model is the
//! expressiveness floor, docs/transforms.md). This is pure policy in
//! the D23 sense: it decides which entries a view WANTS; nothing here
//! touches bytes, and a bad choice costs curation, never data.
//!
//! v1 surface: ordered region priority + ordered language priority over
//! clone families. Families come from the dat's resolved `cloneof`
//! links when the dat has any; flat dats (standard No-Intro) fall back
//! to igir-style inference — entries sharing a base name (everything
//! before the first parenthetical, case-folded) are one family.
//! Two modes per view (D57): **held-first** (default) — a
//! held-and-verified candidate outranks the preferred-but-absent
//! region (the NAS serves what exists; re-eval upgrades picks as
//! holdings improve); **strict** — retool semantics, a pure function
//! of (dat, preferences), for curation publication and want-lists.
//! Retool clonelists ride as an additive family input in both modes
//! ([`crate::clonelist`]).

use std::collections::{HashMap, HashSet};

/// Ordered priorities. Empty lists mean "no preference on that axis".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionPolicy {
    /// e.g. `["USA", "Europe", "Japan"]`; matched against the dat's
    /// parenthetical region tokens (tiny alias table: US/EU/JP).
    pub regions: Vec<String>,
    /// e.g. `["En"]`; matched against `(En,Fr,De)`-style token groups.
    pub langs: Vec<String>,
    /// D57 strict mode: selection is a pure function of
    /// (dat, preferences), independent of holdings — the preferred
    /// entry wins even when absent (its slots render as missing; a
    /// strict view's gaps ARE the want list). Default (`false`) is
    /// held-first: a held-and-verified clone outranks the
    /// preferred-but-absent region.
    pub strict: bool,
}

/// One entry as the selector sees it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub entry_id: i64,
    pub name: String,
    /// Resolved parent entry id when the dat declares clones.
    pub cloneof_id: Option<i64>,
    /// Required (non-optional, non-nodump) claim count.
    pub required: u64,
    /// Of those, how many have a verified held blob.
    pub held: u64,
}

/// A retool-style clonelist (D57, additive input): case-folded search
/// terms → group name. Groups override both the dat's clone graph and
/// base-name inference for the entries they name; everything else
/// falls back as before.
pub type Clonelist = HashMap<String, String>;

/// Pick one entry per clone family. Deterministic: candidates are
/// scored on (fully-held*, dev-flag, region rank, language rank,
/// revision DESC, name) and the winner is unique for a given input.
/// (*held rank participates only in held-first mode; strict mode is a
/// pure function of the dat and the preferences, D57.)
#[must_use]
pub fn select_1g1r(
    candidates: &[Candidate],
    policy: &SelectionPolicy,
    clonelist: Option<&Clonelist>,
) -> HashSet<i64> {
    // Family construction: an explicit clonelist group wins; then the
    // dat's clone graph as soon as it demonstrates one; only fully-flat
    // dats get name inference (mixing declared links with inference
    // would let a coincidental base-name merge two families the dat
    // says are distinct).
    let dat_has_clones = candidates.iter().any(|c| c.cloneof_id.is_some());
    let mut families: HashMap<FamilyKey, Vec<&Candidate>> = HashMap::new();
    for c in candidates {
        let key = if let Some(group) = clonelist.and_then(|cl| lookup_group(cl, &c.name)) {
            FamilyKey::Group(group.clone())
        } else if dat_has_clones {
            FamilyKey::Id(c.cloneof_id.unwrap_or(c.entry_id))
        } else {
            FamilyKey::Name(base_name(&c.name))
        };
        families.entry(key).or_default().push(c);
    }
    families
        .into_values()
        .map(|mut family| {
            family.sort_by_key(|c| score(c, policy));
            family[0].entry_id
        })
        .collect()
}

/// Clonelist lookup: the full name (case-folded) first, then the
/// tag-stripped short name — retool's two non-regex `nameType`s.
fn lookup_group<'a>(clonelist: &'a Clonelist, name: &str) -> Option<&'a String> {
    clonelist
        .get(&name.to_lowercase())
        .or_else(|| clonelist.get(&base_name(name)))
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum FamilyKey {
    Id(i64),
    Name(String),
    Group(String),
}

type Score = (u8, u8, usize, usize, std::cmp::Reverse<u32>, String);

fn score(c: &Candidate, policy: &SelectionPolicy) -> Score {
    let tokens = paren_tokens(&c.name);
    // Strict mode (D57): holdings never influence the pick.
    let fully_held = if policy.strict {
        0
    } else {
        u8::from(!(c.required > 0 && c.held == c.required))
    };
    let dev_flag = u8::from(tokens.iter().any(|t| is_dev_flag(t)));
    let region = axis_rank(&tokens, &policy.regions, normalize_region);
    let lang = axis_rank(&tokens, &policy.langs, str::to_ascii_lowercase);
    (
        fully_held,
        dev_flag,
        region,
        lang,
        std::cmp::Reverse(revision(&tokens)),
        c.name.clone(),
    )
}

/// Base name for family inference: everything before the first `(`,
/// case-folded, whitespace-collapsed.
fn base_name(name: &str) -> String {
    let base = name.split('(').next().unwrap_or(name);
    base.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// All comma-separated tokens across every parenthetical group:
/// `"Game (USA) (En,Fr) (Rev 2)"` → `["USA", "En", "Fr", "Rev 2"]`.
fn paren_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut rest = name;
    while let Some(open) = rest.find('(') {
        let Some(close) = rest[open..].find(')') else {
            break;
        };
        for token in rest[open + 1..open + close].split(',') {
            let token = token.trim();
            if !token.is_empty() {
                tokens.push(token.to_owned());
            }
        }
        rest = &rest[open + close + 1..];
    }
    tokens
}

/// Position of the best-ranked match in an ordered priority list, or
/// one past the end when nothing (or nothing on this axis) matches.
fn axis_rank(
    tokens: &[String],
    priorities: &[String],
    normalize: impl Fn(&str) -> String,
) -> usize {
    priorities
        .iter()
        .position(|want| {
            let want = normalize(want);
            tokens.iter().any(|t| normalize(t) == want)
        })
        .unwrap_or(priorities.len())
}

fn normalize_region(token: &str) -> String {
    let token = token.to_ascii_lowercase();
    match token.as_str() {
        "us" => "usa".to_owned(),
        "eu" => "europe".to_owned(),
        "jp" => "japan".to_owned(),
        _ => token,
    }
}

/// Development/incomplete flags rank behind any production release.
fn is_dev_flag(token: &str) -> bool {
    let token = token.to_ascii_lowercase();
    [
        "beta",
        "proto",
        "prototype",
        "sample",
        "demo",
        "alpha",
        "debug",
    ]
    .iter()
    .any(|flag| token == *flag || token.starts_with(&format!("{flag} ")))
}

/// `(Rev 2)` → 2, `(Rev B)` → 2; absent → 0. Higher wins.
fn revision(tokens: &[String]) -> u32 {
    for token in tokens {
        let lower = token.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("rev ") {
            let rest = rest.trim();
            if let Ok(n) = rest.parse::<u32>() {
                return n;
            }
            let mut chars = rest.chars();
            if let (Some(c), None) = (chars.next(), chars.next())
                && c.is_ascii_alphabetic()
            {
                return u32::from(c.to_ascii_lowercase() as u8 - b'a') + 1;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: i64, name: &str, held: bool) -> Candidate {
        Candidate {
            entry_id: id,
            name: name.into(),
            cloneof_id: None,
            required: 1,
            held: u64::from(held),
        }
    }

    fn policy(regions: &[&str], langs: &[&str]) -> SelectionPolicy {
        SelectionPolicy {
            regions: regions.iter().map(|s| (*s).to_owned()).collect(),
            langs: langs.iter().map(|s| (*s).to_owned()).collect(),
            strict: false,
        }
    }

    #[test]
    fn flat_dat_infers_families_and_ranks_regions() {
        let cands = vec![
            cand(1, "Game, The (Europe)", true),
            cand(2, "Game, The (USA)", true),
            cand(3, "Game, The (Japan)", true),
            cand(4, "Other Game (Japan)", true),
        ];
        let picked = select_1g1r(&cands, &policy(&["USA", "Europe", "Japan"], &[]), None);
        assert!(picked.contains(&2), "USA outranks");
        assert!(picked.contains(&4), "each family yields one");
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn held_beats_preferred_region() {
        let cands = vec![cand(1, "Game (USA)", false), cand(2, "Game (Europe)", true)];
        let picked = select_1g1r(&cands, &policy(&["USA", "Europe"], &[]), None);
        assert!(picked.contains(&2), "the NAS serves what it holds");
    }

    #[test]
    fn dev_flags_lose_to_production() {
        let cands = vec![
            cand(1, "Game (USA) (Beta)", true),
            cand(2, "Game (Europe)", true),
        ];
        let picked = select_1g1r(&cands, &policy(&["USA", "Europe"], &[]), None);
        assert!(picked.contains(&2), "retail Europe over USA beta");
    }

    #[test]
    fn language_breaks_region_ties_and_rev_breaks_language() {
        let cands = vec![
            cand(1, "Game (Europe) (Fr,De)", true),
            cand(2, "Game (Europe) (En,Fr)", true),
        ];
        let picked = select_1g1r(&cands, &policy(&["Europe"], &["En"]), None);
        assert!(picked.contains(&2));

        let cands = vec![
            cand(1, "Game (Europe) (En)", true),
            cand(2, "Game (Europe) (En) (Rev 2)", true),
            cand(3, "Game (Europe) (En) (Rev 1)", true),
        ];
        let picked = select_1g1r(&cands, &policy(&["Europe"], &["En"]), None);
        assert!(picked.contains(&2), "highest revision wins");
    }

    #[test]
    fn declared_clones_override_name_inference() {
        let mut a = cand(1, "Parent (USA)", true);
        a.cloneof_id = None;
        let mut b = cand(2, "Completely Different Name (Europe)", true);
        b.cloneof_id = Some(1);
        // A third entry whose base name collides with nothing declared:
        // with a clone graph present it is its own family.
        let c = cand(3, "Parent (Japan)", true);
        let picked = select_1g1r(&[a, b, c], &policy(&["USA", "Europe", "Japan"], &[]), None);
        assert!(picked.contains(&1), "family {{1,2}} picks USA parent");
        assert!(picked.contains(&3), "undeclared sibling stays separate");
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn region_aliases_and_rev_letters() {
        assert_eq!(normalize_region("EU"), "europe");
        assert_eq!(revision(&["Rev B".to_owned()]), 2);
        assert_eq!(revision(&["Rev 10".to_owned()]), 10);
        assert_eq!(revision(&["USA".to_owned()]), 0);
    }

    #[test]
    fn deterministic_tiebreak_by_name() {
        let cands = vec![
            cand(2, "Game (World) (B-Side)", true),
            cand(1, "Game (World) (A-Side)", true),
        ];
        let picked = select_1g1r(&cands, &policy(&[], &[]), None);
        assert!(picked.contains(&1), "lexicographic name is the last axis");
    }

    #[test]
    fn strict_mode_ignores_holdings() {
        // Same input where held-first picks Europe: strict picks the
        // preferred-but-absent USA — its gaps are the want list (D57).
        let cands = vec![cand(1, "Game (USA)", false), cand(2, "Game (Europe)", true)];
        let mut p = policy(&["USA", "Europe"], &[]);
        p.strict = true;
        let picked = select_1g1r(&cands, &p, None);
        assert!(picked.contains(&1), "pure function of (dat, preferences)");
        // And it's holdings-independent: flipping held bits changes nothing.
        let cands2 = vec![cand(1, "Game (USA)", true), cand(2, "Game (Europe)", false)];
        assert_eq!(select_1g1r(&cands2, &p, None), picked);
    }

    #[test]
    fn clonelist_groups_override_inference() {
        // Base-name inference sees two families; the clonelist says the
        // JP rename is the same game.
        let cands = vec![
            cand(1, "Game, The (USA)", true),
            cand(2, "Gamu za Best (Japan)", true),
        ];
        let picked = select_1g1r(&cands, &policy(&["USA", "Japan"], &[]), None);
        assert_eq!(picked.len(), 2, "without the clonelist: two families");

        let mut cl = Clonelist::new();
        cl.insert("game, the".into(), "Game, The".into());
        cl.insert("gamu za best".into(), "Game, The".into());
        let picked = select_1g1r(&cands, &policy(&["USA", "Japan"], &[]), Some(&cl));
        assert_eq!(picked.len(), 1, "clonelist merges the rename");
        assert!(picked.contains(&1), "USA preferred within the group");
    }

    #[test]
    fn clonelist_composes_with_declared_clones() {
        // The dat declares a clone pair; the clonelist names a THIRD
        // entry the dat missed. Grouped entries leave the dat graph.
        let a = cand(1, "Parent (USA)", true);
        let mut b = cand(2, "Child (Europe)", true);
        b.cloneof_id = Some(1);
        let c = cand(3, "Totally Renamed (Japan)", true);
        let mut cl = Clonelist::new();
        cl.insert("parent".into(), "Parent".into());
        cl.insert("totally renamed".into(), "Parent".into());
        let picked = select_1g1r(&[a, b, c], &policy(&["USA"], &[]), Some(&cl));
        // Parent + Renamed share a clonelist group (winner: USA parent);
        // Child keeps its declared family, now parentless from the
        // group's perspective — it still yields exactly one pick.
        assert!(picked.contains(&1));
        assert_eq!(picked.len(), 2);
    }
}
