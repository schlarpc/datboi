//! The serving-side shape of a view snapshot: decoded manifest rows in a
//! path-ordered map, plus directory semantics derived from row paths.
//!
//! Snapshots are immutable (docs/80-views.md), so a decoded index is
//! cacheable forever by snapshot hash; directories exist implicitly as
//! path prefixes — there are no directory objects to get out of sync.

use std::collections::BTreeMap;

use datboi_core::hash::Blake3;
use datboi_core::viewsnap::ViewSnapshot;

/// What a serving surface needs to know about one manifest row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowMeta {
    pub hash: Blake3,
    pub size: u64,
    /// D27 seek class recorded at snapshot time (0 affine /
    /// 1 manifest-seekable / 2 opaque).
    pub seek: u8,
}

/// A decoded, path-indexed snapshot manifest.
pub(crate) struct ViewIndex {
    pub snapshot: Blake3,
    pub view_name: String,
    pub created_at: u64,
    rows: BTreeMap<String, RowMeta>,
}

/// One directory level: immediate subdirectory names and files.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Listing {
    pub dirs: Vec<String>,
    pub files: Vec<(String, RowMeta)>,
}

impl ViewIndex {
    pub fn from_snapshot(snapshot: Blake3, snap: ViewSnapshot) -> Self {
        let rows = snap
            .rows
            .into_iter()
            .map(|r| {
                (
                    r.path,
                    RowMeta {
                        hash: r.hash,
                        size: r.size,
                        seek: r.seek,
                    },
                )
            })
            .collect();
        Self {
            snapshot,
            view_name: snap.view_name,
            created_at: snap.created_at,
            rows,
        }
    }

    /// Exact-path file lookup. Manifest paths are canonical
    /// (no `.`/`..`/empty components), so a hostile request path can
    /// only ever fail to match — nothing here touches a filesystem.
    pub fn file(&self, path: &str) -> Option<RowMeta> {
        self.rows.get(path).copied()
    }

    /// Does `prefix` name a directory ("" is the root)?
    pub fn is_dir(&self, prefix: &str) -> bool {
        if prefix.is_empty() {
            return true;
        }
        let start = format!("{prefix}/");
        self.rows
            .range(start.clone()..)
            .next()
            .is_some_and(|(k, _)| k.starts_with(&start))
    }

    /// Immediate children of a directory. Assumes `is_dir(prefix)`.
    pub fn list(&self, prefix: &str) -> Listing {
        let start = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        let mut out = Listing::default();
        let mut last_dir: Option<&str> = None;
        for (path, meta) in self.rows.range(start.clone()..) {
            let Some(rest) = path.strip_prefix(&start) else {
                break;
            };
            match rest.split_once('/') {
                Some((child, _)) => {
                    // rows are path-sorted, so a subdirectory's rows are
                    // contiguous: dedup against the previous child only.
                    if last_dir != Some(child) {
                        out.dirs.push(child.to_owned());
                        last_dir = Some(child);
                    }
                }
                None => out.files.push((rest.to_owned(), *meta)),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datboi_core::viewsnap::ViewRow;

    fn index() -> ViewIndex {
        let snap = ViewSnapshot {
            created_at: 1,
            view_name: "v".into(),
            sources: vec![],
            rows: vec![
                row("Alpha/alpha.gba"),
                row("Alpha/sub/deep.bin"),
                row("Beta/beta.gba"),
                row("loose.txt"),
            ],
        };
        ViewIndex::from_snapshot(Blake3::compute(b"snap"), snap)
    }

    fn row(path: &str) -> ViewRow {
        ViewRow {
            path: path.into(),
            hash: Blake3::compute(path.as_bytes()),
            size: 7,
            seek: 0,
        }
    }

    #[test]
    fn files_dirs_and_listings() {
        let idx = index();
        assert!(idx.file("Alpha/alpha.gba").is_some());
        assert!(idx.file("Alpha").is_none(), "dirs are not files");
        assert!(idx.is_dir(""));
        assert!(idx.is_dir("Alpha"));
        assert!(idx.is_dir("Alpha/sub"));
        assert!(!idx.is_dir("Alpha/alpha.gba"));
        assert!(!idx.is_dir("Alp"), "prefix of a name is not a dir");

        let root = idx.list("");
        assert_eq!(root.dirs, vec!["Alpha", "Beta"]);
        assert_eq!(root.files.len(), 1);
        assert_eq!(root.files[0].0, "loose.txt");

        let alpha = idx.list("Alpha");
        assert_eq!(alpha.dirs, vec!["sub"]);
        assert_eq!(alpha.files[0].0, "alpha.gba");
    }
}
