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
//! A READDIR of a view directory is the one place those two classes
//! meet: the directory is named by view, the cookies it hands out are
//! snapshot-keyed. D127 resolves it in the cookie's favour — a cookie
//! IS a fileid, so the snapshot an in-progress walk is reading is
//! recoverable from the walk itself, and a flip mid-enumeration serves
//! the old tree to completion instead of invalidating every
//! outstanding cookie.
//!
//! ## Identity is derived, not allocated (D129)
//!
//! A fileid is a hash of what it names — `(snapshot, path)`, or a
//! view's name — never a counter. Every process that serves this tree
//! therefore agrees on every id without persisting anything, and an id
//! can never come to mean a different node than it did before a
//! restart.
//!
//! The opaque file handle carries that same identity on the wire
//! (class byte, snapshot, 128-bit key) instead of upstream's startup
//! generation number over a process-local id. So a handle a client
//! cached before a deploy still names the file it named: a restart is
//! invisible where it used to be `ESTALE` on every path until someone
//! remounted.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use datboi_core::hash::Blake3;
use datboi_nfs_server::nfs::{
    cookieverf3, fattr3, fileid3, filename3, ftype3, nfs_fh3, nfspath3, nfsstat3, nfstime3, sattr3,
};
use datboi_nfs_server::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use crate::App;
use crate::vfs::{self, LookupError, ViewIndex};

const ROOT_ID: fileid3 = 1;

/// Handle classes — the first byte of every opaque file handle.
const FH_ROOT: u8 = 0;
const FH_VIEW: u8 = 1;
const FH_PATH: u8 = 2;
/// An id this process cannot derive a handle for (see
/// [`NfsFs::id_to_fh`]). Process-local, which is what every handle was
/// before D129.
const FH_OPAQUE: u8 = 3;

/// A node's identity key: the leading 16 bytes of its identity hash.
///
/// A `fileid3` has room for 64 bits of it and a handle has room for
/// more, so it uses more — matching a cold handle back to a path is
/// exact at 128 bits even where the fileid alone could have collided.
type NodeKey = [u8; 16];

/// What a fileid names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Node {
    Root,
    /// A view directory: resolved through its tag on every access.
    View(String),
    /// A snapshot-pinned path (file or directory).
    Path(Blake3, String),
}

/// A handle decoded off the wire whose node this process has not met
/// yet: it names the node exactly, but the path behind the key has not
/// been recovered from the snapshot. Always a handle a client minted
/// before this process started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cold {
    View(NodeKey),
    Path(Blake3, NodeKey),
}

/// The identity key of a path within a snapshot — what a `Node::Path`
/// handle carries, and where that node's fileid comes from. `probe` is
/// the clash escape hatch in [`IdTable::id_for`] and is 0 for every
/// identity that reaches a handle.
fn path_key(snapshot: &Blake3, path: &str, probe: u64) -> NodeKey {
    let mut buf = Vec::with_capacity(16 + 32 + path.len() + 8);
    buf.extend_from_slice(b"datboi-nfs/path\0");
    buf.extend_from_slice(&snapshot.0);
    buf.extend_from_slice(path.as_bytes());
    buf.extend_from_slice(&probe.to_le_bytes());
    truncate(&Blake3::compute(&buf))
}

/// The identity key of a view directory: its NAME, so the id survives
/// every snapshot flip underneath it — the view half of the identity
/// model above. Domain-separated from [`path_key`], so a view named
/// `x` and a path spelled `x` cannot collide by construction.
fn view_key(name: &str, probe: u64) -> NodeKey {
    let mut buf = Vec::with_capacity(16 + name.len() + 8);
    buf.extend_from_slice(b"datboi-nfs/view\0");
    buf.extend_from_slice(name.as_bytes());
    buf.extend_from_slice(&probe.to_le_bytes());
    truncate(&Blake3::compute(&buf))
}

fn truncate(hash: &Blake3) -> NodeKey {
    hash.0[..16].try_into().expect("16 of 32 bytes")
}

/// `None` for the root, whose id is fixed by the protocol rather than
/// derived.
fn key_of(node: &Node, probe: u64) -> Option<NodeKey> {
    match node {
        Node::Root => None,
        Node::View(name) => Some(view_key(name, probe)),
        Node::Path(snapshot, path) => Some(path_key(snapshot, path, probe)),
    }
}

/// The fileid a key names: its low 64 bits, kept clear of 0 (the
/// start-of-directory cookie) and of [`ROOT_ID`].
fn id_of_key(key: &NodeKey) -> fileid3 {
    let id = u64::from_le_bytes(key[..8].try_into().expect("8 of 16 bytes"));
    if id <= ROOT_ID { id + ROOT_ID + 1 } else { id }
}

#[derive(Default)]
struct IdTable {
    by_id: HashMap<fileid3, Node>,
    by_node: HashMap<Node, fileid3>,
}

impl IdTable {
    fn new() -> Self {
        let mut table = Self::default();
        table.by_id.insert(ROOT_ID, Node::Root);
        table.by_node.insert(Node::Root, ROOT_ID);
        table
    }

    /// The fileid for `node`, DERIVED from the node itself (D129) —
    /// the same in every process, with nothing persisted.
    ///
    /// A `fileid3` is 64 bits wide, so a million-row view has roughly a
    /// 1-in-10^8 chance that two of its paths hash alike. Rare is not
    /// never, and an id that names two nodes would serve one file's
    /// bytes under the other's name, so a clash re-derives under the
    /// next probe and this table stays bijective. The loser of a clash
    /// is then the one node whose id depends on mint order; its handle
    /// still carries the full 128-bit key, and [`NfsFs::node`] refuses
    /// such a handle rather than resolving it to the wrong node.
    fn id_for(&mut self, node: &Node) -> fileid3 {
        if let Some(id) = self.by_node.get(node) {
            return *id;
        }
        let Some(key) = key_of(node, 0) else {
            return ROOT_ID; // pre-bound by `new`; unreachable in practice
        };
        let mut id = id_of_key(&key);
        let mut probe = 0u64;
        while self.by_id.get(&id).is_some_and(|held| held != node) {
            probe += 1;
            id = id_of_key(&key_of(node, probe).expect("root is bound, never probed"));
        }
        self.by_id.insert(id, node.clone());
        self.by_node.insert(node.clone(), id);
        id
    }

    fn node(&self, id: fileid3) -> Option<Node> {
        self.by_id.get(&id).cloned()
    }
}

/// Decoded-but-unresolved handles held at once — see
/// [`NfsFs::remember_cold`].
const COLD_HANDLE_CEILING: usize = 64 * 1024;

/// Cached child entries, summed across every cached directory, before
/// the listing cache is dropped wholesale. A view root of ~37k entries
/// is a few MB, so this is tens of MB at the ceiling; entries are
/// derived from immutable snapshots, so a drop only ever costs a
/// rebuild (see [`Listing`]).
const LISTING_CACHE_ENTRIES: usize = 250_000;

/// One directory's children, already sorted, with their fileids.
///
/// Built once per `(snapshot, path)` and kept: a snapshot is immutable
/// (docs/views.md), so a listing over one can never go stale and needs
/// no invalidation. The cache is a performance device ONLY —
/// [`IdTable`] mints ids deterministically, so a rebuild after a drop
/// yields the identical entries under the identical cookies. Dropping
/// it can slow a walk; it can never break one.
struct Listing {
    entries: Vec<(fileid3, Child)>,
    /// cookie -> position. Resuming a walk was a linear scan of this
    /// vector, which is what made a full enumeration quadratic in the
    /// directory (D127).
    by_id: HashMap<fileid3, usize>,
}

pub(crate) struct NfsFs {
    app: Arc<App>,
    ids: Mutex<IdTable>,
    /// Child listings by `(snapshot, path)` — see [`Listing`].
    listings: Mutex<HashMap<(Blake3, String), Arc<Listing>>>,
    /// Handles decoded off the wire that this process has not matched
    /// to a node yet — see [`NfsFs::node`]. An entry lives only until
    /// the node behind it is recovered.
    cold: Mutex<HashMap<fileid3, Cold>>,
    /// Every node key a snapshot contains, by snapshot — how a cold
    /// handle finds its path without a walk. See [`snapshot_keys`].
    keys: Mutex<HashMap<Blake3, Arc<HashMap<NodeKey, String>>>>,
    /// Listings actually built from an index. A full enumeration costs
    /// one; it used to cost one per READDIR call.
    builds: AtomicU64,
}

impl NfsFs {
    pub(crate) fn new(app: Arc<App>) -> Self {
        Self {
            app,
            ids: Mutex::new(IdTable::new()),
            listings: Mutex::new(HashMap::new()),
            cold: Mutex::new(HashMap::new()),
            keys: Mutex::new(HashMap::new()),
            builds: AtomicU64::new(0),
        }
    }

    fn id_for(&self, node: &Node) -> fileid3 {
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .id_for(node)
    }

    /// The node a fileid names.
    ///
    /// A fileid this process minted is in the table. One that arrived
    /// on a handle from a previous process is not — D129 makes that
    /// recoverable rather than `ESTALE`: the handle carries the node's
    /// key, and the snapshot it belongs to is immutable, so the path
    /// behind the key can simply be looked up again.
    async fn node(&self, id: fileid3) -> Result<Node, nfsstat3> {
        if let Some(node) = self
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .node(id)
        {
            return Ok(node);
        }
        let cold = *self
            .cold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .ok_or(nfsstat3::NFS3ERR_STALE)?;
        // Resolved or not, the note has done its job: an entry lives
        // for one operation, so the map is bounded by requests in
        // flight rather than by handles ever seen.
        let thawed = self.thaw(cold).await;
        self.cold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        let node = thawed?;
        if self.id_for(&node) != id {
            // This process probed the node around a fileid clash, so
            // the handle's 64-bit half no longer names it. The handle
            // is honest and the node is real, but serving it under an
            // id that means something else here is exactly the aliasing
            // D129 exists to prevent. Refuse instead.
            return Err(nfsstat3::NFS3ERR_STALE);
        }
        Ok(node)
    }

    /// Note a handle whose node is not known yet, and return the
    /// fileid it claims. Never displaces a live binding.
    fn remember_cold(&self, id: fileid3, cold: Cold) -> fileid3 {
        if self
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .node(id)
            .is_some()
        {
            return id;
        }
        let mut notes = self
            .cold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The write-shaped ops answer `NFS3ERR_ROFS` without ever
        // resolving the handle they were given, so a client that only
        // ever sends those leaks a note per call. A valve, not a
        // policy: reaching it needs more unresolved handles at once
        // than any real client holds.
        if notes.len() >= COLD_HANDLE_CEILING {
            notes.clear();
        }
        notes.insert(id, cold);
        id
    }

    /// Match a cold handle's key back to the node it names.
    async fn thaw(&self, cold: Cold) -> Result<Node, nfsstat3> {
        match cold {
            // Views are few and the tag list is the authority on which
            // exist, so this is a scan rather than a map.
            Cold::View(key) => {
                self.blocking(move |app| {
                    vfs::view_tags(&app)
                        .map_err(|e| map_lookup(&e))?
                        .into_iter()
                        .map(|(name, _)| name)
                        .find(|name| view_key(name, 0) == key)
                        .map(Node::View)
                        .ok_or(nfsstat3::NFS3ERR_STALE)
                })
                .await
            }
            Cold::Path(snapshot, key) => {
                let keys = self.key_map(snapshot).await?;
                keys.get(&key)
                    .map(|path| Node::Path(snapshot, path.clone()))
                    .ok_or(nfsstat3::NFS3ERR_STALE)
            }
        }
    }

    /// A snapshot's node keys, from cache or built. Same bargain as
    /// [`Listing`]: a snapshot is immutable, so this never invalidates
    /// and dropping it only costs a rebuild.
    async fn key_map(&self, snapshot: Blake3) -> Result<Arc<HashMap<NodeKey, String>>, nfsstat3> {
        if let Some(hit) = self
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&snapshot)
        {
            return Ok(Arc::clone(hit));
        }
        let built = self
            .blocking(move |app| {
                let idx = vfs::snapshot_index(&app, snapshot).map_err(|e| map_lookup(&e))?;
                Ok(Arc::new(snapshot_keys(&idx)))
            })
            .await?;
        let mut cache = self
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cached: usize = cache.values().map(|keys| keys.len()).sum();
        if cached >= LISTING_CACHE_ENTRIES {
            cache.clear(); // immutable entries: dropping only costs a rebuild
        }
        cache.insert(snapshot, Arc::clone(&built));
        Ok(built)
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

    /// Mint every child's fileid under one lock hold and index them by
    /// cookie.
    fn materialize(&self, children: Vec<Child>) -> Listing {
        let entries: Vec<(fileid3, Child)> = {
            let mut ids = self
                .ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            children
                .into_iter()
                .map(|child| (ids.id_for(&child.node), child))
                .collect()
        };
        let by_id = entries
            .iter()
            .enumerate()
            .map(|(pos, (id, _))| (*id, pos))
            .collect();
        Listing { entries, by_id }
    }

    /// The children of `path` within `snapshot`, from cache or built.
    async fn listing(&self, snapshot: Blake3, path: String) -> Result<Arc<Listing>, nfsstat3> {
        let key = (snapshot, path);
        if let Some(hit) = self
            .listings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            return Ok(Arc::clone(hit));
        }
        let (snapshot, path) = (key.0, key.1.clone());
        let children = self
            .blocking(move |app| {
                let idx = vfs::snapshot_index(&app, snapshot).map_err(|e| map_lookup(&e))?;
                if !idx.is_dir(&path) {
                    return Err(nfsstat3::NFS3ERR_NOTDIR);
                }
                Ok(listing_nodes(&idx, &path))
            })
            .await?;
        self.builds.fetch_add(1, Ordering::Relaxed);
        let built = Arc::new(self.materialize(children));
        let mut cache = self
            .listings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cached: usize = cache.values().map(|l| l.entries.len()).sum();
        if cached >= LISTING_CACHE_ENTRIES {
            cache.clear(); // immutable entries: dropping only costs a rebuild
        }
        cache.insert(key, Arc::clone(&built));
        Ok(built)
    }

    /// The snapshot an in-progress enumeration of a view root is
    /// already walking, recovered from its own cookie (D127).
    ///
    /// Every child of a view root is `Node::Path(snapshot, name)` with
    /// a single-component name, so the cookie names the tree the client
    /// is mid-way through. Re-resolving the view by name instead would
    /// follow a `view eval` to a new snapshot and strand every
    /// outstanding cookie as `NFS3ERR_BAD_COOKIE`, which a Linux client
    /// can only answer by restarting the walk from zero.
    ///
    /// `None` for the first page (nothing to pin to) and for any cookie
    /// that is not shaped like a view root's child — those fall back to
    /// resolving the view, exactly as before.
    fn pinned_snapshot(&self, start_after: fileid3) -> Option<Blake3> {
        if start_after == 0 {
            return None;
        }
        let held = self
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .node(start_after);
        match held {
            Some(Node::Path(snapshot, path)) if !path.contains('/') => Some(snapshot),
            Some(_) => None,
            // A cookie this process never minted can still name its
            // snapshot, if the client holds a handle for the same
            // entry: a walk interrupted by a restart then resumes
            // where it was instead of starting over (D129).
            None => match self
                .cold
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&start_after)
            {
                Some(Cold::Path(snapshot, _)) => Some(*snapshot),
                _ => None,
            },
        }
    }

    /// Listings built from an index since this filesystem opened.
    #[cfg(test)]
    fn listing_builds(&self) -> u64 {
        self.builds.load(Ordering::Relaxed)
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

/// The export root's children: one directory per `view/` tag, in
/// deterministic (name-sorted) order.
///
/// Never cached — tags are the one mutable thing under this mount, and
/// the listing is O(#views).
fn root_children(app: &App) -> Result<Vec<Child>, nfsstat3> {
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

/// Key every node a snapshot holds: one per manifest row, plus one per
/// directory those rows imply (directories are path prefixes, not
/// objects — see vfs.rs). This is what turns a cold handle's key back
/// into a path, so it has to cover exactly the nodes a handle can name.
///
/// Rows arrive path-sorted, so a directory is new exactly when the
/// previous row was not inside it — one hash per node, not one per
/// node per row.
fn snapshot_keys(idx: &ViewIndex) -> HashMap<NodeKey, String> {
    let mut out = HashMap::new();
    let mut prev = "";
    for (path, _) in idx.rows() {
        for (cut, _) in path.match_indices('/') {
            let dir = &path[..cut];
            let covered = prev.len() > cut && prev.as_bytes()[cut] == b'/' && prev.starts_with(dir);
            if !covered {
                out.insert(path_key(&idx.snapshot, dir, 0), dir.to_owned());
            }
        }
        out.insert(path_key(&idx.snapshot, path, 0), path.to_owned());
        prev = path;
    }
    out
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

    /// The opaque handle for `id`: what it names, not where it was
    /// minted (D129). A class byte plus the identity that class needs —
    /// 49 bytes at most, inside NFSv3's 64.
    ///
    /// Upstream's default is a startup generation number over a
    /// process-local id, which is precisely what made every handle die
    /// with the daemon.
    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        let mut data = Vec::with_capacity(1 + 32 + 16);
        let node = self
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .node(id);
        match node {
            Some(Node::Root) => data.push(FH_ROOT),
            Some(Node::View(name)) => {
                data.push(FH_VIEW);
                data.extend_from_slice(&view_key(&name, 0));
            }
            Some(Node::Path(snapshot, path)) => {
                data.push(FH_PATH);
                data.extend_from_slice(&snapshot.0);
                data.extend_from_slice(&path_key(&snapshot, &path, 0));
            }
            // An id this process never minted has no identity to
            // derive from. Hand back the process-local form rather than
            // a handle that would name something else.
            None => {
                data.push(FH_OPAQUE);
                data.extend_from_slice(&id.to_le_bytes());
            }
        }
        nfs_fh3 { data }
    }

    /// Decode a handle without touching the store: the fileid is the
    /// low half of the key the handle already carries. Recovering the
    /// path behind that key is deferred to [`NfsFs::node`], which can
    /// do it off the reactor.
    fn fh_to_id(&self, fh: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        let (class, rest) = fh.data.split_first().ok_or(nfsstat3::NFS3ERR_BADHANDLE)?;
        match (*class, rest.len()) {
            (FH_ROOT, 0) => Ok(ROOT_ID),
            (FH_VIEW, 16) => {
                let key: NodeKey = rest.try_into().expect("checked length");
                Ok(self.remember_cold(id_of_key(&key), Cold::View(key)))
            }
            (FH_PATH, 48) => {
                let snapshot = Blake3(rest[..32].try_into().expect("checked length"));
                let key: NodeKey = rest[32..].try_into().expect("checked length");
                Ok(self.remember_cold(id_of_key(&key), Cold::Path(snapshot, key)))
            }
            (FH_OPAQUE, 8) => Ok(u64::from_le_bytes(rest.try_into().expect("checked length"))),
            _ => Err(nfsstat3::NFS3ERR_BADHANDLE),
        }
    }

    /// The readdir cookie verifier. Upstream derives it from the
    /// server's startup time, which tells every client its cookies died
    /// with the last process. Since D129 they did not: a cookie is a
    /// derived fileid, and a walk resuming into a directory that no
    /// longer holds it is still answered `NFS3ERR_BAD_COOKIE` by the
    /// listing itself. So the verifier is constant — a restart is not a
    /// reason to make a client re-walk a 37k-entry directory.
    fn serverid(&self) -> cookieverf3 {
        *b"datboi\0\x01"
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let dir = self.node(dirid).await?;
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
        let node = self.node(id).await?;
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
        let Node::Path(snapshot, path) = self.node(id).await? else {
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
        let listing = match self.node(dirid).await? {
            Node::Root => {
                let children = self.blocking(|app| root_children(&app)).await?;
                self.builds.fetch_add(1, Ordering::Relaxed);
                Arc::new(self.materialize(children))
            }
            Node::Path(snapshot, path) => self.listing(snapshot, path).await?,
            // D127: a walk already under way stays on the tree it
            // started on, named by its own cookie. Only a fresh walk
            // (or a cookie that cannot name one) resolves the view.
            Node::View(name) => {
                let snapshot = match self.pinned_snapshot(start_after) {
                    Some(snapshot) => snapshot,
                    None => {
                        self.blocking(move |app| {
                            vfs::view_index(&app, &name)
                                .map(|idx| idx.snapshot)
                                .map_err(|e| map_lookup(&e))
                        })
                        .await?
                    }
                };
                self.listing(snapshot, String::new()).await?
            }
        };
        let skip = if start_after == 0 {
            0
        } else {
            match listing.by_id.get(&start_after) {
                Some(pos) => pos + 1,
                None => return Err(nfsstat3::NFS3ERR_BAD_COOKIE),
            }
        };
        let window = &listing.entries[skip.min(listing.entries.len())..];
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
    use datboi_nfs_server::nfs::NFS3_FHSIZE;
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

    /// D129: an id is a function of the node, so two processes over
    /// one tree agree on every id without having agreed on anything.
    #[test]
    fn a_fileid_is_a_function_of_what_it_names() {
        let snap = Blake3::compute(b"snap");
        let (a, b) = (
            Node::Path(snap, "x/y".into()),
            Node::Path(snap, "x/z".into()),
        );
        let mut first = IdTable::new();
        let (ida, idb) = (first.id_for(&a), first.id_for(&b));

        // mint order differs; the ids do not
        let mut second = IdTable::new();
        assert_eq!(second.id_for(&b), idb, "id follows the node, not the order");
        assert_eq!(second.id_for(&a), ida);

        // a view's id follows its NAME, so it outlives every flip
        let view = Node::View("arcade".into());
        assert_eq!(first.id_for(&view), second.id_for(&view));

        // nothing derives onto a reserved id
        for id in [ida, idb, first.id_for(&view)] {
            assert!(id > ROOT_ID, "{id} collides with a reserved id");
        }
    }

    fn mint_snapshot(store: &Store, db: &Db, rows: Vec<ViewRow>, created_at: u64) -> Blake3 {
        // `created_at` reaches the tag, not the manifest (D118): the
        // snapshot hash is a function of its rows alone.
        let snap = ViewSnapshot {
            created_at_v1: 0,
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

    /// A daemon over a fresh tempdir, with `view/test` tagged at a
    /// snapshot of whatever `build` puts in the store.
    fn app_over(
        build: impl FnOnce(&Store, &Db) -> Vec<ViewRow>,
    ) -> (tempfile::TempDir, Arc<App>, Blake3) {
        let root = tempfile::tempdir().expect("tempdir");
        let store_root = root.path().join("store");
        let db_dir = root.path().join("db");
        std::fs::create_dir_all(&db_dir).expect("db dir");
        let snapshot = {
            let store = Store::open(&store_root).expect("store");
            let db = Db::open(&db_dir).expect("db");
            let rows = build(&store, &db);
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
        (root, app, snapshot)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt")
    }

    /// Every name a client sees walking `dir` in pages of `page`, plus
    /// the number of READDIR calls it took.
    async fn walk(fs: &NfsFs, dir: fileid3, page: usize) -> Result<(Vec<String>, u64), nfsstat3> {
        let mut names = Vec::new();
        let mut calls = 0;
        let mut cookie = 0;
        loop {
            let result = fs.readdir(dir, cookie, page).await?;
            calls += 1;
            for entry in &result.entries {
                names.push(String::from_utf8(entry.name.0.clone()).expect("utf8"));
            }
            if result.end {
                return Ok((names, calls));
            }
            cookie = result
                .entries
                .last()
                .expect("a non-final page is non-empty")
                .fileid;
        }
    }

    /// A plain NFSv3 READDIR walk must terminate and visit each entry
    /// exactly once.
    ///
    /// The READDIRPLUS path (`readdir`) was always covered above; this
    /// one dispatches through `readdir_simple`, which is what the Linux
    /// client actually uses once it stops asking for attributes. Upstream
    /// nfsserve 0.11.0 discarded the cookie there and restarted at entry
    /// 0 on every call, so this walk never ended — against a real view
    /// root of ~36,000 entries, `ls` spun at 100% CPU forever without
    /// issuing another RPC. See crates/datboi-nfs-server/FORK.md.
    #[test]
    fn a_plain_readdir_walk_terminates() {
        let (_root, app, _snap) = app_over(|store, db| {
            vec![
                row(store, db, "a.bin", b"a"),
                row(store, db, "b.bin", b"b"),
                row(store, db, "c.bin", b"c"),
                row(store, db, "d.bin", b"d"),
                row(store, db, "e.bin", b"e"),
            ]
        });
        let fs = NfsFs::new(Arc::clone(&app));
        rt().block_on(async {
            let view_id = fs
                .lookup(ROOT_ID, &"test".as_bytes().into())
                .await
                .expect("view");

            let mut seen: Vec<String> = Vec::new();
            let mut cookie: fileid3 = 0;
            // Two at a time, so resumption is exercised rather than
            // sidestepped by a single page that happens to hold everything.
            for _ in 0..16 {
                let page = fs
                    .readdir_simple(view_id, cookie, 2)
                    .await
                    .expect("readdir_simple");
                for e in &page.entries {
                    seen.push(String::from_utf8(e.name.0.clone()).expect("utf8"));
                }
                if page.end {
                    break;
                }
                let last = page.entries.last().expect("a non-final page has entries");
                assert_ne!(last.fileid, cookie, "walk failed to advance");
                cookie = last.fileid;
            }

            assert_eq!(
                seen,
                vec!["a.bin", "b.bin", "c.bin", "d.bin", "e.bin"],
                "every entry exactly once, in order, and the walk ended"
            );
        });
    }

    /// Walk root → view → dir → file, read with offsets, paginate
    /// readdir, refuse writes, and hold old-snapshot ids across a flip.
    #[test]
    fn trait_surface_over_a_real_snapshot() {
        let content = b"nfs served bytes!".as_slice();
        let (_root, app, snap1) = app_over(|store, db| {
            vec![
                row(store, db, "Dir/a.bin", content),
                row(store, db, "Dir/b.bin", b"bee"),
                row(store, db, "top.bin", b"top"),
            ]
        });
        let fs = NfsFs::new(Arc::clone(&app));
        rt().block_on(async {
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

    /// D127: a `view eval` mid-walk does not strand the walk.
    ///
    /// The view ROOT is the directory anyone actually lists, and it is
    /// the one named by view rather than by snapshot — so before D127
    /// the second page after a flip resolved a different tree, found no
    /// cookie in it, and answered `NFS3ERR_BAD_COOKIE`, whose only
    /// legal client response is to restart from zero.
    #[test]
    fn a_view_root_walk_is_pinned_to_the_snapshot_it_started_on() {
        let content = b"delta bytes".as_slice();
        let (_root, app, snap1) = app_over(|store, db| {
            vec![
                row(store, db, "alpha/rom.bin", b"a"),
                row(store, db, "bravo/rom.bin", b"b"),
                row(store, db, "charlie/rom.bin", b"c"),
                row(store, db, "delta.bin", content),
            ]
        });
        let fs = NfsFs::new(Arc::clone(&app));
        rt().block_on(async {
            let view_id = fs
                .lookup(ROOT_ID, &"test".as_bytes().into())
                .await
                .expect("view");
            let delta_id = fs
                .lookup(view_id, &"delta.bin".as_bytes().into())
                .await
                .expect("delta");

            // half a walk
            let page1 = fs.readdir(view_id, 0, 2).await.expect("page 1");
            assert_eq!((page1.entries.len(), page1.end), (2, false));
            assert_eq!(page1.entries[0].name.0, b"alpha");
            assert_eq!(page1.entries[1].name.0, b"bravo");
            let cookie = page1.entries[1].fileid;

            // ...and the view flips underneath it, to a disjoint tree.
            let snap2 = {
                let db = app.db.lock().unwrap();
                let store = app.store;
                let rows = vec![row(store, &db, "zulu/rom.bin", b"z")];
                mint_snapshot(store, &db, rows, 1_780_000_100)
            };
            assert_ne!(snap1, snap2);

            // the rest of the walk completes, on the tree it started on
            let page2 = fs.readdir(view_id, cookie, 10).await.expect("page 2");
            assert!(page2.end);
            let rest: Vec<&[u8]> = page2.entries.iter().map(|e| e.name.0.as_slice()).collect();
            assert_eq!(rest, vec![b"charlie".as_slice(), b"delta.bin".as_slice()]);

            // D33 is not weakened: the id walked through the old tree
            // still reads the old bytes.
            let (bytes, eof) = fs.read(delta_id, 0, 4096).await.expect("old id reads");
            assert_eq!((bytes.as_slice(), eof), (content, true));

            // and a FRESH walk sees the new tree — pinning binds an
            // enumeration, not the view.
            let (names, _) = walk(&fs, view_id, 10).await.expect("fresh walk");
            assert_eq!(names, vec!["zulu"]);

            // a cookie that names nothing is still a bad cookie.
            assert!(matches!(
                fs.readdir(view_id, 424_242, 10).await,
                Err(nfsstat3::NFS3ERR_BAD_COOKIE)
            ));
        });
    }

    /// D129: a handle minted before a restart still names its file.
    ///
    /// Every deploy used to invalidate every handle every client held,
    /// with no self-healing — the arcade cabinet's symptom was `Stale
    /// file handle` on every path until someone dropped the mount. And
    /// because ids came from a counter, nothing but luck stopped a
    /// reused integer from naming a DIFFERENT file after the restart.
    /// So this walks the second process onto other nodes first, which
    /// is exactly what used to consume the cached handle's integer.
    #[test]
    fn a_handle_outlives_the_process_that_minted_it() {
        let content = b"bytes that outlive a deploy".as_slice();
        let (_root, app, _snap) = app_over(|store, db| {
            vec![
                row(store, db, "Dir/a.bin", content),
                row(store, db, "Dir/b.bin", b"bee"),
                row(store, db, "top.bin", b"top"),
            ]
        });
        let rt = rt();

        // what a client caches while the first daemon is up
        let (root_fh, view_fh, dir_fh, file_fh) = rt.block_on(async {
            let fs = NfsFs::new(Arc::clone(&app));
            let view = fs
                .lookup(ROOT_ID, &"test".as_bytes().into())
                .await
                .expect("view");
            let dir = fs
                .lookup(view, &"Dir".as_bytes().into())
                .await
                .expect("dir");
            let file = fs
                .lookup(dir, &"a.bin".as_bytes().into())
                .await
                .expect("file");
            let fhs = (
                fs.id_to_fh(ROOT_ID),
                fs.id_to_fh(view),
                fs.id_to_fh(dir),
                fs.id_to_fh(file),
            );
            for fh in [&fhs.0, &fhs.1, &fhs.2, &fhs.3] {
                assert!(
                    fh.data.len() <= NFS3_FHSIZE as usize,
                    "handle overruns NFS3_FHSIZE"
                );
            }
            fhs
        });

        // the deploy: same store on disk, brand new server state
        let fs = NfsFs::new(Arc::clone(&app));
        rt.block_on(async {
            assert_eq!(fs.fh_to_id(&root_fh).expect("root handle"), ROOT_ID);

            // the view directory resolves by name, through its tag
            let view = fs.fh_to_id(&view_fh).expect("view handle");
            assert!(matches!(
                fs.getattr(view).await.expect("view attr").ftype,
                ftype3::NF3DIR
            ));

            // mint some ids the way a fresh client would, first
            let top = fs
                .lookup(view, &"top.bin".as_bytes().into())
                .await
                .expect("top");
            let (bytes, _) = fs.read(top, 0, 4096).await.expect("read top");
            assert_eq!(bytes.as_slice(), b"top");

            // the cached file handle still means the file it meant
            let file = fs.fh_to_id(&file_fh).expect("file handle");
            assert_ne!(file, top, "two nodes, two ids");
            let (bytes, eof) = fs.read(file, 0, 4096).await.expect("read");
            assert_eq!(
                (bytes.as_slice(), eof),
                (content, true),
                "same handle, same bytes"
            );

            // ...and so does a directory's, which is a path PREFIX
            // rather than a manifest row (vfs.rs), so it has to be
            // keyed too or `ls` of a cached subdirectory breaks alone.
            let dir = fs.fh_to_id(&dir_fh).expect("dir handle");
            let (names, _) = walk(&fs, dir, 10).await.expect("walk");
            assert_eq!(names, vec!["a.bin", "b.bin"]);
        });
    }

    /// D129: a handle that names nothing is refused, never resolved to
    /// whatever is nearby.
    #[test]
    fn an_unresolvable_handle_is_stale_not_somebody_else() {
        let (_root, app, snap) = app_over(|store, db| vec![row(store, db, "top.bin", b"top")]);
        let fs = NfsFs::new(Arc::clone(&app));
        rt().block_on(async {
            // well-formed, correctly snapshot-scoped, names no node
            let mut data = vec![FH_PATH];
            data.extend_from_slice(&snap.0);
            data.extend_from_slice(&path_key(&snap, "ghost.bin", 0));
            let ghost = fs.fh_to_id(&nfs_fh3 { data }).expect("well-formed");
            assert!(matches!(
                fs.getattr(ghost).await,
                Err(nfsstat3::NFS3ERR_STALE)
            ));

            // a view that no tag names any more
            let mut data = vec![FH_VIEW];
            data.extend_from_slice(&view_key("retired", 0));
            let retired = fs.fh_to_id(&nfs_fh3 { data }).expect("well-formed");
            assert!(matches!(
                fs.getattr(retired).await,
                Err(nfsstat3::NFS3ERR_STALE)
            ));

            // and garbage is refused at the door
            for data in [vec![], vec![FH_PATH], vec![9, 9, 9]] {
                assert!(matches!(
                    fs.fh_to_id(&nfs_fh3 { data }),
                    Err(nfsstat3::NFS3ERR_BADHANDLE)
                ));
            }
        });
    }

    /// D129 + D127: a restart mid-walk does not restart the walk, even
    /// when the deploy that caused it also flipped the view.
    ///
    /// The cookie is a derived fileid, so the new process reads it the
    /// same way the old one wrote it; the handle the client holds for
    /// that same entry still names its snapshot, so D127's pinning
    /// survives the process that started the walk.
    #[test]
    fn a_walk_resumes_across_a_restart() {
        let (_root, app, snap1) = app_over(|store, db| {
            vec![
                row(store, db, "alpha/rom.bin", b"a"),
                row(store, db, "bravo/rom.bin", b"b"),
                row(store, db, "charlie/rom.bin", b"c"),
                row(store, db, "delta.bin", b"d"),
            ]
        });
        let rt = rt();

        // half a walk, then the client caches what it has
        let (view_fh, cookie, cookie_fh) = rt.block_on(async {
            let fs = NfsFs::new(Arc::clone(&app));
            let view = fs
                .lookup(ROOT_ID, &"test".as_bytes().into())
                .await
                .expect("view");
            let page1 = fs.readdir(view, 0, 2).await.expect("page 1");
            assert_eq!((page1.entries.len(), page1.end), (2, false));
            let cookie = page1.entries[1].fileid;
            (fs.id_to_fh(view), cookie, fs.id_to_fh(cookie))
        });

        // the deploy, which also flips the view to a disjoint tree
        let snap2 = {
            let db = app.db.lock().unwrap();
            let store = app.store;
            let rows = vec![row(store, &db, "zulu/rom.bin", b"z")];
            mint_snapshot(store, &db, rows, 1_780_000_100)
        };
        assert_ne!(snap1, snap2);

        let fs = NfsFs::new(Arc::clone(&app));
        rt.block_on(async {
            let view = fs.fh_to_id(&view_fh).expect("view handle");
            // the client holds a handle for the entry it stopped at
            fs.fh_to_id(&cookie_fh).expect("cookie handle");

            let page2 = fs.readdir(view, cookie, 10).await.expect("page 2");
            assert!(page2.end);
            let rest: Vec<&[u8]> = page2.entries.iter().map(|e| e.name.0.as_slice()).collect();
            assert_eq!(rest, vec![b"charlie".as_slice(), b"delta.bin".as_slice()]);

            // a walk that starts here sees the new tree, as always
            let (names, _) = walk(&fs, view, 10).await.expect("fresh walk");
            assert_eq!(names, vec!["zulu"]);
        });
    }

    /// D127: a full enumeration costs ONE child-list build, not one per
    /// call. `listing_builds` is counted rather than timed so the bound
    /// is exact — before D127 this was 256 builds of a 4,096-entry
    /// directory, i.e. quadratic in the directory.
    #[test]
    fn a_full_view_root_walk_builds_one_listing() {
        const SETS: usize = 4_096;
        const PAGE: usize = 16;
        let (_root, app, _snap) = app_over(|_store, _db| {
            // Rows need no blobs behind them: readdir reads the
            // manifest and never touches a byte of content.
            (0..SETS)
                .map(|i| {
                    let path = format!("set{i:06}/rom.bin");
                    ViewRow {
                        hash: Blake3::compute(path.as_bytes()),
                        path,
                        size: 1024,
                        seek: 0,
                    }
                })
                .collect()
        });
        let fs = NfsFs::new(Arc::clone(&app));
        rt().block_on(async {
            let view_id = fs
                .lookup(ROOT_ID, &"test".as_bytes().into())
                .await
                .expect("view");
            let (names, calls) = walk(&fs, view_id, PAGE).await.expect("walk");
            assert_eq!(names.len(), SETS, "every set, exactly once");
            assert!(names.windows(2).all(|w| w[0] < w[1]), "sorted, no repeats");
            assert_eq!(calls, (SETS / PAGE) as u64, "pages of PAGE");
            assert_eq!(
                fs.listing_builds(),
                1,
                "{calls} calls must not cost {calls} builds"
            );
        });
    }
}
