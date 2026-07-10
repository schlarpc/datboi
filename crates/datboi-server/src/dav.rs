//! WebDAV surface (docs/80-views.md: dav-server, day one) mounted at
//! `/dav`: the collection root lists views, each view is the D33
//! tag-resolved snapshot tree. Strictly read-only — the method set is
//! WEBDAV_RO (OPTIONS/GET/HEAD/PROPFIND) and every write-shaped
//! filesystem op answers Forbidden, so no client can mutate a snapshot
//! through protocol jank.
//!
//! Reads go through the same [`Executor::serve_range`] path as plain
//! HTTP (D49 verified windows); dav-server only supplies the protocol
//! engine, never bytes.

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use datboi_core::hash::Blake3;
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use dav_server::{DavHandler, DavMethodSet};

use crate::App;
use crate::vfs::{self, LookupError, RowMeta, ViewIndex};

/// Build the `/dav`-rooted protocol handler.
pub(crate) fn handler(app: Arc<App>) -> DavHandler {
    DavHandler::builder()
        .filesystem(Box::new(DavFs { app }))
        .strip_prefix("/dav")
        .methods(DavMethodSet::WEBDAV_RO)
        // Each read is one verified serve_range call (route planning
        // included) — make them big enough to amortize.
        .read_buf_size(1 << 20)
        .autoindex(true)
        .build_handler()
}

#[derive(Clone)]
struct DavFs {
    app: Arc<App>,
}

/// A resolved DAV path.
enum Node {
    Root,
    Dir(Arc<ViewIndex>, String),
    File(Arc<ViewIndex>, RowMeta),
}

fn map_lookup(e: &LookupError) -> FsError {
    match e {
        LookupError::NoSuchView => FsError::NotFound,
        _ => FsError::GeneralFailure,
    }
}

/// Resolve a decoded, relative DAV path against the view roots.
/// Lookups are exact string matches into canonical manifests — no
/// filesystem semantics can leak in.
fn resolve(app: &App, rel: &str) -> FsResult<Node> {
    let rel = rel.trim_matches('/');
    if rel.is_empty() {
        return Ok(Node::Root);
    }
    let (view, rest) = rel.split_once('/').unwrap_or((rel, ""));
    let idx = vfs::view_index(app, view).map_err(|e| map_lookup(&e))?;
    if rest.is_empty() {
        return Ok(Node::Dir(idx, String::new()));
    }
    if let Some(row) = idx.file(rest) {
        return Ok(Node::File(idx, row));
    }
    if idx.is_dir(rest) {
        let rest = rest.to_owned();
        return Ok(Node::Dir(idx, rest));
    }
    Err(FsError::NotFound)
}

fn rel_str(path: &DavPath) -> FsResult<String> {
    path.as_rel_ospath()
        .to_str()
        .map(str::to_owned)
        .ok_or(FsError::NotFound)
}

impl DavFileSystem for DavFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            if options.write
                || options.append
                || options.truncate
                || options.create
                || options.create_new
            {
                return Err(FsError::Forbidden);
            }
            let app = Arc::clone(&self.app);
            let rel = rel_str(path)?;
            let node = blocking(move || resolve(&app, &rel)).await?;
            match node {
                Node::File(idx, row) => Ok(Box::new(RangeFile {
                    app: Arc::clone(&self.app),
                    hash: row.hash,
                    size: row.size,
                    created_at: idx.created_at,
                    pos: 0,
                }) as Box<dyn DavFile>),
                // Collections are listed via read_dir, never opened.
                Node::Root | Node::Dir(..) => Err(FsError::NotFound),
            }
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let app = Arc::clone(&self.app);
            let rel = rel_str(path)?;
            let entries = blocking(move || dir_entries(&app, &rel)).await?;
            Ok(Box::pin(IterStream(entries.into_iter())) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let app = Arc::clone(&self.app);
            let rel = rel_str(path)?;
            blocking(move || {
                let meta = match resolve(&app, &rel)? {
                    Node::Root => Meta {
                        len: 0,
                        modified_unix: 0,
                        is_dir: true,
                        etag: None,
                    },
                    Node::Dir(idx, _) => Meta::dir(&idx),
                    Node::File(idx, row) => Meta::file(&idx, row),
                };
                Ok(Box::new(meta) as Box<dyn DavMetaData>)
            })
            .await
        })
    }
}

fn dir_entries(app: &App, rel: &str) -> FsResult<Vec<FsResult<Box<dyn DavDirEntry>>>> {
    let mut out: Vec<FsResult<Box<dyn DavDirEntry>>> = Vec::new();
    match resolve(app, rel)? {
        Node::Root => {
            for (name, _) in vfs::view_tags(app).map_err(|e| map_lookup(&e))? {
                // Resolve each view for its snapshot mtime; a broken
                // view shouldn't hide its siblings, so surface it as a
                // per-entry error only if the tag no longer resolves.
                match vfs::view_index(app, &name) {
                    Ok(idx) => out.push(Ok(Box::new(Entry {
                        name,
                        meta: Meta::dir(&idx),
                    }))),
                    Err(e) => out.push(Err(map_lookup(&e))),
                }
            }
        }
        Node::Dir(idx, prefix) => {
            let listing = idx.list(&prefix);
            for dir in listing.dirs {
                out.push(Ok(Box::new(Entry {
                    name: dir,
                    meta: Meta::dir(&idx),
                })));
            }
            for (name, row) in listing.files {
                out.push(Ok(Box::new(Entry {
                    name,
                    meta: Meta::file(&idx, row),
                })));
            }
        }
        Node::File(..) => return Err(FsError::NotFound),
    }
    Ok(out)
}

// ---- metadata / entries ----

#[derive(Debug, Clone)]
struct Meta {
    len: u64,
    modified_unix: u64,
    is_dir: bool,
    /// Content-hash ETag for files (strong; dav-server adds quotes).
    etag: Option<String>,
}

impl Meta {
    fn dir(idx: &ViewIndex) -> Self {
        Self {
            len: 0,
            modified_unix: idx.created_at,
            is_dir: true,
            etag: None,
        }
    }

    fn file(idx: &ViewIndex, row: RowMeta) -> Self {
        Self {
            len: row.size,
            modified_unix: idx.created_at,
            is_dir: false,
            etag: Some(row.hash.to_hex()),
        }
    }
}

impl DavMetaData for Meta {
    fn len(&self) -> u64 {
        self.len
    }
    fn modified(&self) -> FsResult<std::time::SystemTime> {
        Ok(UNIX_EPOCH + Duration::from_secs(self.modified_unix))
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
    fn etag(&self) -> Option<String> {
        self.etag.clone()
    }
}

struct Entry {
    name: String,
    meta: Meta,
}

impl DavDirEntry for Entry {
    fn name(&self) -> Vec<u8> {
        self.name.clone().into_bytes()
    }
    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(std::future::ready(Ok(
            Box::new(self.meta.clone()) as Box<dyn DavMetaData>
        )))
    }
}

// ---- the read-only file handle ----

struct RangeFile {
    app: Arc<App>,
    hash: Blake3,
    size: u64,
    created_at: u64,
    pos: u64,
}

impl std::fmt::Debug for RangeFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RangeFile")
            .field("hash", &self.hash)
            .field("size", &self.size)
            .field("pos", &self.pos)
            .finish_non_exhaustive()
    }
}

impl DavFile for RangeFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = Meta {
            len: self.size,
            modified_unix: self.created_at,
            is_dir: false,
            etag: Some(self.hash.to_hex()),
        };
        Box::pin(std::future::ready(Ok(
            Box::new(meta) as Box<dyn DavMetaData>
        )))
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, bytes::Bytes> {
        Box::pin(async move {
            let want = (count as u64).min(self.size.saturating_sub(self.pos));
            if want == 0 {
                return Ok(bytes::Bytes::new());
            }
            let app = Arc::clone(&self.app);
            let (hash, pos) = (self.hash, self.pos);
            let bytes = blocking(move || {
                let db = app
                    .db
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                app.exec
                    .serve_range(&db, &hash, pos, want)
                    .map_err(|_| FsError::GeneralFailure)
            })
            .await?;
            self.pos += bytes.len() as u64;
            Ok(bytes::Bytes::from(bytes))
        })
    }

    fn seek(&'_ mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(std::future::ready({
            use std::io::SeekFrom;
            let next = match pos {
                SeekFrom::Start(n) => Some(n),
                SeekFrom::End(delta) => self.size.checked_add_signed(delta),
                SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
            };
            match next {
                Some(n) => {
                    self.pos = n;
                    Ok(n)
                }
                None => Err(FsError::GeneralFailure),
            }
        }))
    }

    fn write_buf(&'_ mut self, _buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn write_bytes(&'_ mut self, _buf: bytes::Bytes) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

// ---- plumbing ----

/// Run blocking store/index work off the reactor.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> FsResult<T> + Send + 'static,
) -> FsResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| FsError::GeneralFailure)?
}

/// A ready-made entry list as a Stream (dav-server's expected shape).
struct IterStream(std::vec::IntoIter<FsResult<Box<dyn DavDirEntry>>>);

impl futures_core::Stream for IterStream {
    type Item = FsResult<Box<dyn DavDirEntry>>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(self.0.next())
    }
}
