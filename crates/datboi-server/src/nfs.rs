//! In-process userspace NFSv3 (docs/views.md: the primary mount;
//! D32 userspace-only serving). Read-only over the same snapshot VFS
//! as HTTP/DAV; every file read is an [`Executor::serve_range`] call —
//! the D49 verified path.
//!
//! ## Identity model (the D33 promise under a stateless protocol)
//!
//! NFSv3 has no opens, only 64-bit fileids. Two node classes:
//!
//! - a VIEW directory's fileid names the view (stable across snapshot
//!   flips) and resolves through its tag at access time;
//! - everything beneath is keyed `(snapshot, path)` — the fileid a
//!   client walked to is pinned to the snapshot it walked through, so
//!   an eval mid-read never changes bytes under an already-held id.
//!   Old-snapshot ids keep serving as long as the bytes resolve (CAS
//!   makes "the old tree" free).
//!
//! Ids are allocated per process; nfsserve's generation number stales
//! all handles across daemon restarts, which is ordinary NFS behavior.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use datboi_core::hash::Blake3;
use nfsserve::nfs::{fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use crate::App;
use crate::vfs::{self, LookupError, ViewIndex};

const ROOT_ID: fileid3 = 1;

/// What a fileid names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Node {
    Root,
    /// A view directory: resolved through its tag on every access.
    View(String),
    /// A snapshot-pinned path (file or directory).
    Path(Blake3, String),
}

#[derive(Default)]
struct IdTable {
    by_id: HashMap<fileid3, Node>,
    by_node: HashMap<Node, fileid3>,
    next: fileid3,
}

impl IdTable {
    fn new() -> Self {
        let mut table = Self {
            next: ROOT_ID + 1,
            ..Self::default()
        };
        table.by_id.insert(ROOT_ID, Node::Root);
        table.by_node.insert(Node::Root, ROOT_ID);
        table
    }

    fn id_for(&mut self, node: &Node) -> fileid3 {
        if let Some(id) = self.by_node.get(node) {
            return *id;
        }
        let id = self.next;
        self.next += 1;
        self.by_id.insert(id, node.clone());
        self.by_node.insert(node.clone(), id);
        id
    }

    fn node(&self, id: fileid3) -> Option<Node> {
        self.by_id.get(&id).cloned()
    }
}

pub(crate) struct NfsFs {
    app: Arc<App>,
    ids: Mutex<IdTable>,
}

impl NfsFs {
    pub(crate) fn new(app: Arc<App>) -> Self {
        Self {
            app,
            ids: Mutex::new(IdTable::new()),
        }
    }

    fn id_for(&self, node: &Node) -> fileid3 {
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .id_for(node)
    }

    fn node(&self, id: fileid3) -> Result<Node, nfsstat3> {
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .node(id)
            .ok_or(nfsstat3::NFS3ERR_STALE)
    }

    /// Run blocking store/index work off the reactor.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(Arc<App>) -> Result<T, nfsstat3> + Send + 'static,
    ) -> Result<T, nfsstat3> {
        let app = Arc::clone(&self.app);
        tokio::task::spawn_blocking(move || f(app))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?
    }
}

fn map_lookup(e: &LookupError) -> nfsstat3 {
    match e {
        LookupError::NoSuchView => nfsstat3::NFS3ERR_NOENT,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

fn time(seconds_unix: u64) -> nfstime3 {
    nfstime3 {
        seconds: u32::try_from(seconds_unix).unwrap_or(u32::MAX),
        nseconds: 0,
    }
}

fn dir_attr(id: fileid3, mtime_unix: u64) -> fattr3 {
    fattr3 {
        ftype: ftype3::NF3DIR,
        mode: 0o555,
        nlink: 2,
        uid: 0,
        gid: 0,
        size: 4096,
        used: 4096,
        rdev: Default::default(),
        fsid: 0xda7b_0175,
        fileid: id,
        atime: time(mtime_unix),
        mtime: time(mtime_unix),
        ctime: time(mtime_unix),
    }
}

fn file_attr(id: fileid3, size: u64, mtime_unix: u64) -> fattr3 {
    fattr3 {
        ftype: ftype3::NF3REG,
        mode: 0o444,
        nlink: 1,
        uid: 0,
        gid: 0,
        size,
        used: size,
        rdev: Default::default(),
        fsid: 0xda7b_0175,
        fileid: id,
        atime: time(mtime_unix),
        mtime: time(mtime_unix),
        ctime: time(mtime_unix),
    }
}

fn utf8_name(name: &filename3) -> Result<&str, nfsstat3> {
    std::str::from_utf8(name).map_err(|_| nfsstat3::NFS3ERR_NOENT)
}

/// One child as readdir/lookup see it.
struct Child {
    name: String,
    node: Node,
    is_dir: bool,
    size: u64,
    mtime: u64,
}

/// A directory's children in deterministic (name-sorted) order.
fn children_of(app: &App, node: &Node) -> Result<Vec<Child>, nfsstat3> {
    match node {
        Node::Root => {
            let mut views = vfs::view_tags(app).map_err(|e| map_lookup(&e))?;
            views.sort();
            views
                .into_iter()
                .map(|(name, snapshot)| {
                    let idx = vfs::snapshot_index(app, snapshot).map_err(|e| map_lookup(&e))?;
                    Ok(Child {
                        node: Node::View(name.clone()),
                        name,
                        is_dir: true,
                        size: 4096,
                        mtime: idx.created_at,
                    })
                })
                .collect()
        }
        Node::View(name) => {
            let idx = vfs::view_index(app, name).map_err(|e| map_lookup(&e))?;
            Ok(listing_nodes(&idx, ""))
        }
        Node::Path(snapshot, path) => {
            let idx = vfs::snapshot_index(app, *snapshot).map_err(|e| map_lookup(&e))?;
            if !idx.is_dir(path) {
                return Err(nfsstat3::NFS3ERR_NOTDIR);
            }
            Ok(listing_nodes(&idx, path))
        }
    }
}

fn listing_nodes(idx: &ViewIndex, prefix: &str) -> Vec<Child> {
    let listing = idx.list(prefix);
    let join = |name: &str| {
        if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        }
    };
    let mut out: Vec<Child> = Vec::new();
    for dir in listing.dirs {
        out.push(Child {
            node: Node::Path(idx.snapshot, join(&dir)),
            name: dir,
            is_dir: true,
            size: 4096,
            mtime: idx.created_at,
        });
    }
    for (name, meta) in listing.files {
        out.push(Child {
            node: Node::Path(idx.snapshot, join(&name)),
            name,
            is_dir: false,
            size: meta.size,
            mtime: idx.created_at,
        });
    }
    // dirs and files arrive independently sorted; the merged listing
    // must be deterministic for readdir pagination.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[async_trait]
impl NFSFileSystem for NfsFs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadOnly
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_ID
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let dir = self.node(dirid)?;
        let name = utf8_name(filename)?.to_owned();
        if name == "." {
            return Ok(dirid);
        }
        if name == ".." {
            // parent: root is its own parent; views hang off root;
            // paths walk up one component (to the view dir at the top).
            return match &dir {
                Node::Root | Node::View(_) => Ok(ROOT_ID),
                Node::Path(snapshot, path) => match path.rsplit_once('/') {
                    Some((parent, _)) => Ok(self.id_for(&Node::Path(*snapshot, parent.to_owned()))),
                    // top of a snapshot tree: the view dir that got us
                    // here isn't recoverable from the snapshot alone;
                    // fall back to root (clients only use this for cwd
                    // walks, never for reads).
                    None => Ok(ROOT_ID),
                },
            };
        }
        let child = {
            let dir = dir.clone();
            self.blocking(move |app| match &dir {
                Node::Root => {
                    let views = vfs::view_tags(&app).map_err(|e| map_lookup(&e))?;
                    if views.iter().any(|(v, _)| *v == name) {
                        Ok(Node::View(name.clone()))
                    } else {
                        Err(nfsstat3::NFS3ERR_NOENT)
                    }
                }
                Node::View(view) => {
                    let idx = vfs::view_index(&app, view).map_err(|e| map_lookup(&e))?;
                    if idx.file(&name).is_some() || idx.is_dir(&name) {
                        Ok(Node::Path(idx.snapshot, name.clone()))
                    } else {
                        Err(nfsstat3::NFS3ERR_NOENT)
                    }
                }
                Node::Path(snapshot, path) => {
                    let idx = vfs::snapshot_index(&app, *snapshot).map_err(|e| map_lookup(&e))?;
                    let child = format!("{path}/{name}");
                    if idx.file(&child).is_some() || idx.is_dir(&child) {
                        Ok(Node::Path(*snapshot, child))
                    } else {
                        Err(nfsstat3::NFS3ERR_NOENT)
                    }
                }
            })
            .await?
        };
        Ok(self.id_for(&child))
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let node = self.node(id)?;
        self.blocking(move |app| match &node {
            Node::Root => Ok(dir_attr(id, 0)),
            Node::View(name) => {
                let idx = vfs::view_index(&app, name).map_err(|e| map_lookup(&e))?;
                Ok(dir_attr(id, idx.created_at))
            }
            Node::Path(snapshot, path) => {
                let idx = vfs::snapshot_index(&app, *snapshot).map_err(|e| map_lookup(&e))?;
                if let Some(row) = idx.file(path) {
                    Ok(file_attr(id, row.size, idx.created_at))
                } else if idx.is_dir(path) {
                    Ok(dir_attr(id, idx.created_at))
                } else {
                    Err(nfsstat3::NFS3ERR_NOENT)
                }
            }
        })
        .await
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let Node::Path(snapshot, path) = self.node(id)? else {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        };
        self.blocking(move |app| {
            let idx = vfs::snapshot_index(&app, snapshot).map_err(|e| map_lookup(&e))?;
            let row = idx.file(&path).ok_or(nfsstat3::NFS3ERR_ISDIR)?;
            let start = offset.min(row.size);
            let want = u64::from(count).min(row.size - start);
            let bytes = if want == 0 {
                Vec::new()
            } else {
                let db = app.readers.get();
                app.exec
                    .serve_range(&db, &row.hash, start, want)
                    .map_err(|_| nfsstat3::NFS3ERR_IO)?
            };
            let eof = start + bytes.len() as u64 >= row.size;
            Ok((bytes, eof))
        })
        .await
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let dir = self.node(dirid)?;
        let children = self.blocking(move |app| children_of(&app, &dir)).await?;
        // Materialize ids, then window after `start_after`.
        let all: Vec<(fileid3, Child)> = children
            .into_iter()
            .map(|child| (self.id_for(&child.node), child))
            .collect();
        let skip = if start_after == 0 {
            0
        } else {
            match all.iter().position(|(id, _)| *id == start_after) {
                Some(pos) => pos + 1,
                None => return Err(nfsstat3::NFS3ERR_BAD_COOKIE),
            }
        };
        let window = &all[skip.min(all.len())..];
        let end = window.len() <= max_entries;
        let entries = window
            .iter()
            .take(max_entries)
            .map(|(id, child)| DirEntry {
                fileid: *id,
                name: child.name.as_bytes().into(),
                attr: if child.is_dir {
                    dir_attr(*id, child.mtime)
                } else {
                    file_attr(*id, child.size, child.mtime)
                },
            })
            .collect();
        Ok(ReadDirResult { entries, end })
    }

    // ---- write-shaped ops: read-only filesystem ----

    async fn setattr(&self, _id: fileid3, _setattr: sattr3) -> Result<fattr3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn write(&self, _id: fileid3, _offset: u64, _data: &[u8]) -> Result<fattr3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create_exclusive(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn mkdir(
        &self,
        _dirid: fileid3,
        _dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn remove(&self, _dirid: fileid3, _filename: &filename3) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn rename(
        &self,
        _from_dirid: fileid3,
        _from_filename: &filename3,
        _to_dirid: fileid3,
        _to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datboi_core::viewsnap::{ViewRow, ViewSnapshot};
    use datboi_index::{Db, Namespace as IxNs, Residency};
    use datboi_store_fs::{Namespace as StoreNs, Store};

    #[test]
    fn id_table_is_stable_and_bijective() {
        let mut t = IdTable::new();
        let snap = Blake3::compute(b"snap");
        let a = t.id_for(&Node::Path(snap, "x/y".into()));
        let b = t.id_for(&Node::Path(snap, "x/z".into()));
        assert_ne!(a, b);
        assert_eq!(t.id_for(&Node::Path(snap, "x/y".into())), a, "stable");
        assert_eq!(t.node(a), Some(Node::Path(snap, "x/y".into())));
        assert_eq!(t.node(ROOT_ID), Some(Node::Root));
        assert_eq!(t.node(999), None);
    }

    fn mint_snapshot(store: &Store, db: &Db, rows: Vec<ViewRow>, created_at: u64) -> Blake3 {
        let snap = ViewSnapshot {
            created_at,
            view_name: "test".into(),
            sources: vec![],
            rows,
        };
        let encoded = snap.encode().expect("encode");
        let hash = Blake3::compute(&encoded);
        store
            .put(StoreNs::Meta, hash, encoded.as_slice())
            .expect("put snap");
        db.upsert_blob(
            &hash,
            Some(encoded.len() as u64),
            IxNs::Meta,
            Residency::Resident,
        )
        .expect("index");
        db.set_tag("view/test", &hash, i64::try_from(created_at).unwrap())
            .expect("tag");
        hash
    }

    fn row(store: &Store, db: &Db, path: &str, bytes: &[u8]) -> ViewRow {
        let hash = Blake3::compute(bytes);
        store
            .put_with_obao(StoreNs::Data, hash, bytes.len() as u64, bytes)
            .expect("put");
        db.upsert_blob(
            &hash,
            Some(bytes.len() as u64),
            IxNs::Data,
            Residency::Resident,
        )
        .expect("index");
        ViewRow {
            path: path.into(),
            hash,
            size: bytes.len() as u64,
            seek: 0,
        }
    }

    /// Walk root → view → dir → file, read with offsets, paginate
    /// readdir, refuse writes, and hold old-snapshot ids across a flip.
    #[test]
    fn trait_surface_over_a_real_snapshot() {
        let root = tempfile::tempdir().expect("tempdir");
        let store_root = root.path().join("store");
        let db_dir = root.path().join("db");
        std::fs::create_dir_all(&db_dir).expect("db dir");
        let content = b"nfs served bytes!".as_slice();
        let snap1 = {
            let store = Store::open(&store_root).expect("store");
            let db = Db::open(&db_dir).expect("db");
            let rows = vec![
                row(&store, &db, "Dir/a.bin", content),
                row(&store, &db, "Dir/b.bin", b"bee"),
                row(&store, &db, "top.bin", b"top"),
            ];
            mint_snapshot(&store, &db, rows, 1_780_000_000)
        };
        let app = App::open(&crate::Config {
            store_root,
            db_dir,
            listen: "127.0.0.1:0".parse().expect("addr"),
            nfs_listen: None,
            detectors_dir: None,
            refine: false,
            p2p: false,
        })
        .expect("app");
        let fs = NfsFs::new(Arc::clone(&app));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            // walk down
            let view_id = fs
                .lookup(ROOT_ID, &"test".as_bytes().into())
                .await
                .expect("view");
            let dir_id = fs
                .lookup(view_id, &"Dir".as_bytes().into())
                .await
                .expect("dir");
            let file_id = fs
                .lookup(dir_id, &"a.bin".as_bytes().into())
                .await
                .expect("file");
            assert!(matches!(
                fs.lookup(ROOT_ID, &"nope".as_bytes().into()).await,
                Err(nfsstat3::NFS3ERR_NOENT)
            ));

            // attributes
            let attr = fs.getattr(file_id).await.expect("attr");
            assert_eq!(attr.size, content.len() as u64);
            assert!(matches!(attr.ftype, ftype3::NF3REG));
            let attr = fs.getattr(dir_id).await.expect("attr");
            assert!(matches!(attr.ftype, ftype3::NF3DIR));

            // reads: middle window, EOF clamp, past-EOF
            let (bytes, eof) = fs.read(file_id, 4, 6).await.expect("read");
            assert_eq!((bytes.as_slice(), eof), (&content[4..10], false));
            let (bytes, eof) = fs.read(file_id, 10, 4096).await.expect("read");
            assert_eq!((bytes.as_slice(), eof), (&content[10..], true));
            let (bytes, eof) = fs.read(file_id, 4096, 10).await.expect("read");
            assert_eq!((bytes.len(), eof), (0, true));
            assert!(matches!(
                fs.read(dir_id, 0, 10).await,
                Err(nfsstat3::NFS3ERR_ISDIR)
            ));

            // readdir pagination: 1 entry at a time, deterministic
            let page1 = fs.readdir(dir_id, 0, 1).await.expect("readdir");
            assert_eq!((page1.entries.len(), page1.end), (1, false));
            assert_eq!(page1.entries[0].name.0, b"a.bin");
            let page2 = fs
                .readdir(dir_id, page1.entries[0].fileid, 10)
                .await
                .expect("readdir");
            assert_eq!((page2.entries.len(), page2.end), (1, true));
            assert_eq!(page2.entries[0].name.0, b"b.bin");
            assert!(matches!(
                fs.readdir(dir_id, 424_242, 10).await,
                Err(nfsstat3::NFS3ERR_BAD_COOKIE)
            ));

            // read-only, twice over
            assert!(matches!(
                fs.write(file_id, 0, b"nope").await,
                Err(nfsstat3::NFS3ERR_ROFS)
            ));
            assert!(matches!(
                fs.remove(dir_id, &"a.bin".as_bytes().into()).await,
                Err(nfsstat3::NFS3ERR_ROFS)
            ));

            // snapshot flip: the view resolves to a NEW tree, while the
            // already-held file id keeps serving the OLD bytes (D33).
            let snap2 = {
                let db = app.db.lock().unwrap();
                let store = app.store;
                let rows = vec![row(store, &db, "Dir/c.bin", b"sea")];
                mint_snapshot(store, &db, rows, 1_780_000_100)
            };
            assert_ne!(snap1, snap2);
            let dir_id2 = fs
                .lookup(view_id, &"Dir".as_bytes().into())
                .await
                .expect("dir2");
            assert_ne!(dir_id, dir_id2, "new snapshot, new identity");
            let listing = fs.readdir(dir_id2, 0, 10).await.expect("readdir");
            assert_eq!(listing.entries.len(), 1);
            assert_eq!(listing.entries[0].name.0, b"c.bin");
            let (bytes, eof) = fs.read(file_id, 0, 4096).await.expect("old id reads");
            assert_eq!((bytes.as_slice(), eof), (content, true));
        });
    }
}
