//! Constraint profiles (80-views.md): curated bundles of target-device
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
        // filesystem does (80-views.md).
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
        cap_component(&cleaned, self.max_name_len.saturating_sub(SUFFIX_RESERVE))
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

    #[test]
    fn unknown_profile_is_none() {
        assert!(profile("does-not-exist").is_none());
    }
}
