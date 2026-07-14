//! Constraint profiles (views.md): curated bundles of target-device
//! limits applied at view layout time. Knobs live HERE, not scattered
//! (the anti-RetroArch clause) — a view names a profile, the evaluator
//! enforces it, and the report says what the constraints cost.
//!
//! Enforcement semantics:
//! - names: charset-sanitize then length-cap each path component
//!   (deterministically, with room reserved for the collision suffix);
//! - oversize files: the row is SKIPPED and counted — a FAT32 target
//!   cannot hold a >4 GiB file, and silently truncating would be a lie
//!   (auto-split is image-synthesis-era work);
//! - overfull directories: counted and reported, rows kept — dropping
//!   entries is worse than telling the operator their layout template
//!   needs another level.

/// Room reserved in a capped component for the deterministic collision
/// suffix (`" (xxxxxxxx)"`).
const SUFFIX_RESERVE: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    /// Anything canonical (`path_is_canonical`) passes.
    Any,
    /// FAT/exFAT/NTFS-safe: bans control chars and `" * : < > ? \ |`,
    /// plus trailing dots/spaces per component.
    Fat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: &'static str,
    pub charset: Charset,
    /// Max bytes per path component (UTF-8).
    pub max_name_len: usize,
    /// Files larger than this don't fit the target at all.
    pub max_file_size: Option<u64>,
    /// Directories with more children than this are reported.
    pub max_dir_entries: Option<usize>,
}

/// The curated set. Adding a device is adding a row.
pub const PROFILES: &[Profile] = &[
    Profile {
        name: "fat32",
        charset: Charset::Fat,
        max_name_len: 255,
        max_file_size: Some((4 << 30) - 1),
        max_dir_entries: Some(65_534),
    },
    Profile {
        // FAT32 card + console menu: the UI chokes long before the
        // filesystem does (views.md).
        name: "everdrive",
        charset: Charset::Fat,
        max_name_len: 255,
        max_file_size: Some((4 << 30) - 1),
        max_dir_entries: Some(1_000),
    },
    Profile {
        // exFAT cards; no 4 GiB ceiling, same charset caution.
        name: "mister",
        charset: Charset::Fat,
        max_name_len: 255,
        max_file_size: None,
        max_dir_entries: Some(10_000),
    },
    Profile {
        // EZ-Flash Omega (views.md, device data from the 2021
        // prototype): 99-char names, 512 files per directory.
        name: "ezflash-omega",
        charset: Charset::Fat,
        max_name_len: 99,
        max_file_size: Some((4 << 30) - 1),
        max_dir_entries: Some(512),
    },
];

/// Look up a built-in profile by name.
#[must_use]
pub fn profile(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name == name)
}

impl Profile {
    /// Constrain one already-canonical path: per-component charset
    /// sanitation and length cap. Deterministic; never empties a
    /// component.
    #[must_use]
    pub fn constrain_path(&self, path: &str) -> String {
        path.split('/')
            .map(|c| self.constrain_component(c))
            .collect::<Vec<_>>()
            .join("/")
    }

    fn constrain_component(&self, component: &str) -> String {
        let cleaned: String = match self.charset {
            Charset::Any => component.to_owned(),
            Charset::Fat => {
                let mut s: String = component
                    .chars()
                    .map(|c| match c {
                        '"' | '*' | ':' | '<' | '>' | '?' | '\\' | '|' => '_',
                        c if (c as u32) < 0x20 => '_',
                        c => c,
                    })
                    .collect();
                while s.ends_with('.') || s.ends_with(' ') {
                    s.pop();
                }
                s
            }
        };
        let cleaned = if cleaned.is_empty() {
            "_".to_owned()
        } else {
            cleaned
        };
        let budget = self.max_name_len.saturating_sub(SUFFIX_RESERVE);
        let fitted = fit_component(&cleaned, budget);
        cap_component(&fitted, budget)
    }
}

// ---- the name-fitting pipeline (views.md) ----
//
// Length caps are enforced by rewriting, not skipping: an ordered,
// deterministic rule list applied until the name fits, and only then
// the blunt truncate-with-suffix-reserve. A ROM dropped (or mangled)
// for a 103-char dat name is a real loss; the same ROM as "(U)" is
// not. Recovered from the 2021 prototype's EZ-Flash Omega mutator.

/// Single-letter compressions for region/language tags inside
/// parenthesized groups — the GoodTools-style codes.
const REGION_CODES: &[(&str, &str)] = &[
    ("USA", "U"),
    ("Europe", "E"),
    ("Japan", "J"),
    ("World", "W"),
    ("France", "F"),
    ("Germany", "G"),
    ("Spain", "S"),
    ("Italy", "I"),
    ("Australia", "A"),
    ("Korea", "K"),
    ("China", "C"),
    ("Netherlands", "N"),
    ("Brazil", "B"),
];

/// Apply the rewrite rules in order, stopping as soon as the component
/// fits `budget` bytes. Rules never empty a component; a name that
/// still doesn't fit falls through to [`cap_component`].
fn fit_component(component: &str, budget: usize) -> String {
    if component.len() <= budget {
        return component.to_owned();
    }
    // Rule 1: strip noise prefixes ("2 Games in 1! - " and kin).
    let mut name = strip_noise_prefix(component);
    if name.len() <= budget {
        return name;
    }
    // Rule 2: compress region tags ((USA, Europe) → (U,E)).
    name = compress_region_tags(&name);
    if name.len() <= budget {
        return name;
    }
    // Rule 3: trim trailing junk (before the extension).
    trim_trailing_junk(&name)
}

/// `"2 Games in 1! - "`, `"3 in 1 - "`, … — the compilation-cart noise
/// that eats half a name budget while carrying no identity. ASCII
/// case-insensitive cursor over the original string (every matched
/// byte is ASCII, so the final slice is char-boundary safe).
fn strip_noise_prefix(name: &str) -> String {
    let b = name.as_bytes();
    let mut i = 0usize;
    let take = |i: &mut usize, lit: &str| -> bool {
        let end = *i + lit.len();
        if end <= name.len() && name[*i..end].eq_ignore_ascii_case(lit) {
            *i = end;
            true
        } else {
            false
        }
    };
    // <digits> [" games"] " in " <digits> ["!"] " - "
    let d0 = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == d0 {
        return name.to_owned();
    }
    take(&mut i, " games");
    if !take(&mut i, " in ") {
        return name.to_owned();
    }
    let d1 = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == d1 {
        return name.to_owned();
    }
    take(&mut i, "!");
    if !take(&mut i, " - ") {
        return name.to_owned();
    }
    if i >= name.len() {
        return name.to_owned();
    }
    name[i..].to_owned()
}

/// Rewrite every parenthesized group whose comma-separated tokens ALL
/// have single-letter codes: `(USA, Europe)` → `(U,E)`. Mixed groups
/// (revision tags, etc.) stay untouched.
fn compress_region_tags(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(open) = rest.find('(') {
        let Some(close_rel) = rest[open..].find(')') else {
            break;
        };
        let close = open + close_rel;
        out.push_str(&rest[..open]);
        let inner = &rest[open + 1..close];
        let codes: Option<Vec<&str>> = inner
            .split(',')
            .map(|tok| {
                REGION_CODES
                    .iter()
                    .find(|(full, _)| full.eq_ignore_ascii_case(tok.trim()))
                    .map(|(_, code)| *code)
            })
            .collect();
        match codes {
            Some(codes) => {
                out.push('(');
                out.push_str(&codes.join(","));
                out.push(')');
            }
            None => out.push_str(&rest[open..=close]),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Drop trailing separators/empties from the stem: spaces, dots,
/// dashes, and empty `()`/`[]` groups left behind by earlier rules.
fn trim_trailing_junk(name: &str) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, Some(ext)),
        _ => (name, None),
    };
    let mut stem = stem.to_owned();
    loop {
        let before = stem.len();
        while stem.ends_with(' ') || stem.ends_with('.') || stem.ends_with('-') {
            stem.pop();
        }
        for empty in ["()", "[]"] {
            if let Some(s) = stem.strip_suffix(empty) {
                stem = s.to_owned();
            }
        }
        if stem.len() == before || stem.is_empty() {
            break;
        }
    }
    if stem.is_empty() {
        stem = "_".to_owned();
    }
    match ext {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem,
    }
}

/// Cap a component to `max` bytes at a char boundary, preserving the
/// extension when there is one.
fn cap_component(component: &str, max: usize) -> String {
    if component.len() <= max {
        return component.to_owned();
    }
    let (stem, ext) = match component.rsplit_once('.') {
        // ridiculous "extensions" are just long names
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, Some(ext)),
        _ => (component, None),
    };
    let ext_cost = ext.map_or(0, |e| e.len() + 1);
    let budget = max.saturating_sub(ext_cost).max(1);
    let mut cut = budget.min(stem.len());
    while cut > 0 && !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = stem[..cut.max(1)].to_owned();
    if let Some(ext) = ext {
        out.push('.');
        out.push_str(ext);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use datboi_core::viewsnap::path_is_canonical;

    #[test]
    fn fat_charset_scrubs_and_trims() {
        let p = profile("fat32").expect("builtin");
        assert_eq!(p.constrain_path("a:b/c?d.gba"), "a_b/c_d.gba");
        assert_eq!(p.constrain_path("name. /rom.bin"), "name/rom.bin");
        assert_eq!(p.constrain_path("con\ttrol.gba"), "con_trol.gba");
        assert!(path_is_canonical(&p.constrain_path("x./y.")));
    }

    #[test]
    fn long_components_cap_preserving_extension() {
        let p = profile("everdrive").expect("builtin");
        let long = format!("{}.gba", "x".repeat(300));
        let capped = p.constrain_path(&long);
        assert!(capped.len() <= 255 - SUFFIX_RESERVE);
        assert!(capped.ends_with(".gba"));
        // multibyte safety: never cut inside a char
        let uni = format!("{}.gba", "é".repeat(200));
        let capped = p.constrain_path(&uni);
        assert!(capped.len() <= 255 - SUFFIX_RESERVE);
        assert!(capped.ends_with(".gba"));
    }

    #[test]
    fn deterministic_and_stable_for_legal_names() {
        let p = profile("fat32").expect("builtin");
        assert_eq!(p.constrain_path("Alpha (USA).gba"), "Alpha (USA).gba");
        assert_eq!(
            p.constrain_path("Alpha (USA).gba"),
            p.constrain_path("Alpha (USA).gba")
        );
    }

    // ---- the name-fitting pipeline (views.md) ----

    #[test]
    fn fitting_only_runs_when_over_budget() {
        // Under budget: rewrite rules never touch the name, even though
        // rules WOULD apply (region tag present).
        assert_eq!(fit_component("Alpha (USA).gba", 99), "Alpha (USA).gba");
    }

    #[test]
    fn noise_prefixes_strip() {
        assert_eq!(
            strip_noise_prefix("2 Games in 1! - Sonic Advance & ChuChu Rocket! (Europe).gba"),
            "Sonic Advance & ChuChu Rocket! (Europe).gba"
        );
        assert_eq!(
            strip_noise_prefix("3 in 1 - Life, Yahtzee, Payday (USA).gba"),
            "Life, Yahtzee, Payday (USA).gba"
        );
        // Not noise: no marker shape.
        assert_eq!(strip_noise_prefix("1942 (Japan).nes"), "1942 (Japan).nes");
        assert_eq!(
            strip_noise_prefix("2 Fast 2 Furious - The Game.gba"),
            "2 Fast 2 Furious - The Game.gba"
        );
    }

    #[test]
    fn region_tags_compress() {
        assert_eq!(
            compress_region_tags("Game, The (USA, Europe) (Rev 1).gba"),
            "Game, The (U,E) (Rev 1).gba"
        );
        assert_eq!(compress_region_tags("Solo (Japan).gba"), "Solo (J).gba");
        // Mixed/unknown groups stay untouched.
        assert_eq!(
            compress_region_tags("Thing (Beta) (Proto 2).gba"),
            "Thing (Beta) (Proto 2).gba"
        );
    }

    #[test]
    fn trailing_junk_trims() {
        assert_eq!(trim_trailing_junk("Name - .gba"), "Name.gba");
        assert_eq!(trim_trailing_junk("Name ().gba"), "Name.gba");
        assert_eq!(trim_trailing_junk("Name...gba"), "Name.gba");
    }

    #[test]
    fn ezflash_omega_fits_the_2021_shape() {
        // The motivating case: a 103-char compilation name survives the
        // 99-char (minus reserve) budget as MEANING, not truncation.
        let p = profile("ezflash-omega").expect("builtin");
        let long = "2 Games in 1! - Sonic Advance & Sonic Battle Ultra Mega Deluxe Championship Tournament Edition (USA, Europe) (Rev 2).gba";
        assert!(long.len() - "2 Games in 1! - ".len() > 99 - SUFFIX_RESERVE);
        let fitted = p.constrain_path(long);
        assert!(fitted.len() <= 99 - SUFFIX_RESERVE);
        assert!(
            fitted.starts_with("Sonic Advance"),
            "prefix stripped, identity kept: {fitted}"
        );
        assert!(fitted.contains("(U,E)"), "regions compressed: {fitted}");
        assert!(fitted.ends_with(".gba"));
        // Deterministic.
        assert_eq!(fitted, p.constrain_path(long));
    }

    #[test]
    fn unknown_profile_is_none() {
        assert!(profile("does-not-exist").is_none());
    }
}
