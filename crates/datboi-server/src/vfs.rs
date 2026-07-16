//! The serving-side shape of a view snapshot: decoded manifest rows in a
//! path-ordered map, plus directory semantics derived from row paths.
//!
//! Snapshots are immutable (docs/views.md), so a decoded index is
//! cacheable forever by snapshot hash; directories exist implicitly as
//! path prefixes — there are no directory objects to get out of sync.

use std::collections::BTreeMap;
use std::sync::Arc;

use datboi_core::hash::Blake3;
use datboi_core::viewsnap::ViewSnapshot;

use crate::App;

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

/// Why a tree lookup failed — each serving surface maps these to its
/// own status vocabulary (HTTP statuses, DAV FsErrors).
#[derive(Debug)]
pub(crate) enum LookupError {
    NoSuchView,
    /// The snapshot object is not in the meta store.
    SnapshotMissing,
    Corrupt(String),
    Internal(String),
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchView => write!(f, "no such view"),
            Self::SnapshotMissing => write!(f, "snapshot not in store"),
            Self::Corrupt(detail) => write!(f, "snapshot does not decode: {detail}"),
            Self::Internal(detail) => write!(f, "{detail}"),
        }
    }
}

/// All `view/<name>` tags as (name, snapshot hash), the serving roots.
pub(crate) fn view_tags(app: &App) -> Result<Vec<(String, Blake3)>, LookupError> {
    let db = app.readers.get();
    let tags = db
        .list_tags()
        .map_err(|e| LookupError::Internal(e.to_string()))?;
    Ok(tags
        .into_iter()
        .filter_map(|(name, hash)| name.strip_prefix("view/").map(|n| (n.to_owned(), hash)))
        .collect())
}

/// Resolve a view name through its tag (the per-request D33 read).
pub(crate) fn view_index(app: &App, name: &str) -> Result<Arc<ViewIndex>, LookupError> {
    let snapshot = {
        let db = app.readers.get();
        db.get_tag(&format!("view/{name}"))
            .map_err(|e| LookupError::Internal(e.to_string()))?
    };
    snapshot_index(app, snapshot.ok_or(LookupError::NoSuchView)?)
}

/// Load (or hit the cache for) a snapshot's decoded manifest.
pub(crate) fn snapshot_index(app: &App, snapshot: Blake3) -> Result<Arc<ViewIndex>, LookupError> {
    if let Some(idx) = app
        .manifests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&snapshot)
    {
        return Ok(Arc::clone(idx));
    }
    let mut bytes = Vec::new();
    {
        use std::io::Read as _;
        let Some(mut file) = app
            .store
            .get(datboi_store_fs::Namespace::Meta, &snapshot)
            .map_err(|e| LookupError::Internal(e.to_string()))?
        else {
            return Err(LookupError::SnapshotMissing);
        };
        file.read_to_end(&mut bytes)
            .map_err(|e| LookupError::Internal(e.to_string()))?;
    }
    let snap = ViewSnapshot::decode(&bytes).map_err(|e| LookupError::Corrupt(e.to_string()))?;
    let idx = Arc::new(ViewIndex::from_snapshot(snapshot, snap));
    let mut cache = app
        .manifests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.len() >= 64 {
        cache.clear(); // immutable entries: dropping only costs a re-decode
    }
    cache.insert(snapshot, Arc::clone(&idx));
    Ok(idx)
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

    /// Every manifest row in path order — the flat listing the friend
    /// browse surface pages through (api.rs `/v1/views/{name}/files`).
    pub fn rows(&self) -> impl Iterator<Item = (&str, &RowMeta)> {
        self.rows.iter().map(|(path, meta)| (path.as_str(), meta))
    }

    /// (row count, total manifest bytes) — the cheap per-view stats the
    /// M5 API surfaces (decoded once per snapshot via the cache).
    pub fn stats(&self) -> (usize, u64) {
        (
            self.rows.len(),
            self.rows.values().map(|meta| meta.size).sum(),
        )
    }

    /// Does any manifest row reference `hash`? Powers the entry
    /// drawer's "pinned by" list; a linear scan of an already-decoded
    /// manifest is fine at view scale.
    pub fn contains_hash(&self, hash: &Blake3) -> bool {
        self.rows.values().any(|meta| meta.hash == *hash)
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
