//! The datboi daemon (docs/infra.md): axum + tokio, 12-factor config
//! via env, serving view snapshots over HTTP with Range support (M4,
//! docs/views.md). Loopback connections are implicitly owner;
//! binding beyond loopback means non-loopback requests need a session
//! or bearer token (auth v1, D30/D68 — see [`auth`]).
//!
//! Serving surfaces present SNAPSHOTS only (D33): `/view/<name>/…`
//! resolves the `view/<name>` tag per request (so an eval flips the
//! tree atomically between requests, never mid-read — reads hold the
//! resolved snapshot), and `/snap/<hash>/…` addresses any snapshot
//! immutably.

mod admin;
mod analyzers;
mod api;
pub mod auth;
mod compress;
mod dats;
mod dav;
mod emu;
mod gc;
mod hardening;
mod http;
mod ingest;
mod jobs;
mod maintain;
mod nfs;
mod refine;
mod vfs;
mod views;
mod web;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::Db;
use datboi_store_fs::Store;
use tracing::{error, info, warn};

/// Daemon configuration (resolved from flags/`DATBOI_*` env by the CLI).
pub struct Config {
    /// Store root (data/, meta/, tmp/) — may be on NFS.
    pub store_root: PathBuf,
    /// Database directory — local disk, never NFS (D15).
    pub db_dir: PathBuf,
    /// HTTP/WebDAV listen address; loopback unless the operator opted
    /// out.
    pub listen: SocketAddr,
    /// NFSv3 listen address (`None` = NFS off). Consoles need a LAN
    /// bind — but NFS carries no auth (D68 keeps it loopback-only-by-
    /// default in M5), so a wide bind warns loudly.
    pub nfs_listen: Option<SocketAddr>,
    /// Header-skipper detector XML directory (same as the CLI's
    /// `--detectors`); `None` = ingest runs without skipper variants.
    pub detectors_dir: Option<PathBuf>,
    /// Ambient refinement (D71): a niced background worker analyzes
    /// fresh ingests immediately and the corpus backlog continuously.
    /// On by default in `datboi serve`; off in tests that need a
    /// quiescent database.
    pub refine: bool,
    /// p2p seedbox (D97): serve our holdings to peers over iroh, under the
    /// derived iroh identity (D99). Opt-in — off by default; enabling it
    /// opens a network endpoint and joins n0 discovery. A start failure
    /// (e.g. offline) is logged, not fatal.
    pub p2p: bool,
}

/// Read-only connection count. Reads are short and WAL readers never
/// block each other or the writer; four absorbs one slow read without
/// serializing the rest. Molten later if a surface measures a need.
const READ_POOL_SIZE: usize = 4;

/// D93: the request path's READ-ONLY connections. `get` try-locks
/// round-robin and only blocks when every reader is busy. Read-only
/// is enforced at the sqlite flags level ([`Db::open_read_only`]):
/// a misclassified handler errors loudly instead of corrupting.
pub(crate) struct ReadPool {
    conns: Vec<Mutex<Db>>,
    rotor: std::sync::atomic::AtomicUsize,
}

impl ReadPool {
    fn open(db_dir: &std::path::Path, size: usize) -> anyhow::Result<Self> {
        let conns = (0..size)
            .map(|_| Ok(Mutex::new(Db::open_read_only(db_dir)?)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            conns,
            rotor: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub(crate) fn get(&self) -> std::sync::MutexGuard<'_, Db> {
        let start = self
            .rotor
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for i in 0..self.conns.len() {
            if let Ok(guard) = self.conns[(start + i) % self.conns.len()].try_lock() {
                return guard;
            }
        }
        self.conns[start % self.conns.len()]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Quick-write lane size (D93). Two lets a second single-transaction
/// write reach the WAL lock while the first is mid-commit; SQLite
/// serializes the writers themselves, so this is about not queueing a
/// login behind another login, never about parallel writers.
const WRITE_POOL_SIZE: usize = 2;

/// D93: the request path's QUICK-WRITE lane. Auth and admin writes are
/// each a single IMMEDIATE transaction (or an idempotent single
/// statement) over tables — users, sessions, invites, grants — the
/// pipeline writer never touches. Their atomicity is the transaction
/// (IMMEDIATE takes the WAL write lock at BEGIN, so no other writer on
/// ANY connection can interleave until COMMIT), not a process-wide
/// hold — so they pool instead of queueing behind a long dat-import or
/// ingest on the pipeline writer. The per-surface audit that named this
/// safe is D93's owed follow-up, now cashed. Shape mirrors [`ReadPool`],
/// but the connections are read-WRITE.
pub(crate) struct WritePool {
    conns: Vec<Mutex<Db>>,
    rotor: std::sync::atomic::AtomicUsize,
}

impl WritePool {
    fn open(db_dir: &std::path::Path, size: usize) -> anyhow::Result<Self> {
        let conns = (0..size)
            .map(|_| Ok(Mutex::new(Db::open(db_dir)?)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            conns,
            rotor: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub(crate) fn get(&self) -> std::sync::MutexGuard<'_, Db> {
        let start = self
            .rotor
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for i in 0..self.conns.len() {
            if let Ok(guard) = self.conns[(start + i) % self.conns.len()].try_lock() {
                return guard;
            }
        }
        self.conns[start % self.conns.len()]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Shared server state (D93 shape). Three lanes, each with a named
/// argument. The PIPELINE writer (`db`) is serialized in-process: its
/// surfaces — dat import, ingest, gc keep/apply, view eval, snapshot —
/// are multi-transaction sequences that must not interleave with each
/// other while yielding the WAL lock between steps (a single mega-
/// transaction would hold the write lock for a whole 512 MiB import and
/// starve the refiner). The QUICK-WRITE pool (`writers`) carries the
/// single-transaction auth/admin surfaces, whose atomicity is intrinsic
/// (D93's IMMEDIATE default) — they no longer need the process hold the
/// original mutex named. Reads go through the READ-ONLY pool: WAL gives
/// readers snapshot isolation, and the flags-level read-only fence
/// makes a misclassified handler error loudly instead of corrupting.
pub(crate) struct App {
    pub(crate) db: Mutex<Db>,
    pub(crate) writers: WritePool,
    pub(crate) readers: ReadPool,
    pub(crate) exec: Executor<'static>,
    pub(crate) store: &'static Store,
    /// The databases directory — opened as a fresh PRIVATE connection by
    /// on-demand maintenance jobs (scrub/evict/sweep/snapshot), so a
    /// minutes-long corpus walk never holds the pipeline write mutex (the
    /// refiner's D71 posture, reused for request-triggered maintenance).
    pub(crate) db_dir: PathBuf,
    /// Decoded manifests by snapshot hash. Immutable objects, so
    /// entries never invalidate; bounded by wholesale clear.
    pub(crate) manifests: Mutex<HashMap<Blake3, Arc<vfs::ViewIndex>>>,
    /// Staged uploads + the in-memory job registry (web ingest +
    /// refine drains). Arc: the refine worker reports into it from
    /// its own thread.
    pub(crate) jobs: Arc<jobs::Registry>,
    /// Header-skipper detectors, loaded once at open (CLI parity).
    pub(crate) detectors: Vec<datboi_formats::skipper::Detector>,
    /// Ambient refinement worker handle (D71); `None` when disabled.
    /// Ingest completion feeds it fresh blob ids.
    pub(crate) refiner: Option<refine::Refiner>,
    /// Outbound p2p handle (D101), set once by `serve()` when the
    /// seedbox binds — sync rides the seedbox's own endpoint/identity,
    /// so no seedbox means a clean 503 on `POST /v1/p2p/sync`.
    pub(crate) p2p: std::sync::OnceLock<datboi_p2p::P2pClient>,
}

/// A bound-but-not-yet-serving daemon, so callers (and tests) can learn
/// the actual address before requests flow.
pub struct Server {
    listener: std::net::TcpListener,
    nfs_listen: Option<SocketAddr>,
    p2p: bool,
    app: Arc<App>,
}

impl App {
    /// Open store + databases into shared daemon state.
    fn open(config: &Config) -> anyhow::Result<Arc<Self>> {
        let store_root = &config.store_root;
        let store = Store::open(store_root)
            .with_context(|| format!("opening store at {}", store_root.display()))?;
        // The executor borrows the store for its lifetime; the daemon's
        // lifetime IS the process lifetime, so one leaked Store is the
        // honest expression of that (no self-referential gymnastics).
        let store: &'static Store = Box::leak(Box::new(store));
        // Best-effort sweep of crash-orphaned temps — including staged
        // uploads a dead daemon left behind (same 24 h the CLI uses).
        if let Err(e) = store.cleanup_temp(std::time::Duration::from_secs(24 * 60 * 60)) {
            warn!("temp sweep: {e}");
        }
        let detectors = match &config.detectors_dir {
            Some(dir) => {
                let (detectors, errors) = datboi_ingest::load_detectors(dir);
                for (path, err) in errors {
                    warn!("detector {}: {err}", path.display());
                }
                detectors
            }
            None => Vec::new(),
        };
        let db = Db::open(&config.db_dir)
            .with_context(|| format!("opening databases in {}", config.db_dir.display()))?;
        let exec = Executor::new(store, ExecConfig::default())?;
        // The registry's own connection pair (D74): job history writes
        // never contend with the request path's db mutex.
        let jobs = Arc::new(
            jobs::Registry::durable(Db::open(&config.db_dir)?, auth::now_unix())
                .context("hydrating the job ledger")?,
        );
        // Spawned before the first request on purpose: the startup
        // drain covers whatever accumulated while the daemon was down.
        let refiner = config
            .refine
            .then(|| refine::Refiner::spawn(config.db_dir.clone(), store, Arc::clone(&jobs)));
        // After the read-write open: migrations have run, so the quick-
        // write and read-only pools open onto the current schema (the
        // D93 migration-ladder hardening also makes a concurrent open
        // idempotent, but sequencing after the first open keeps it moot).
        let writers =
            WritePool::open(&config.db_dir, WRITE_POOL_SIZE).context("quick-write pool")?;
        let readers = ReadPool::open(&config.db_dir, READ_POOL_SIZE).context("read-only pool")?;
        Ok(Arc::new(App {
            db: Mutex::new(db),
            writers,
            readers,
            exec,
            store,
            db_dir: config.db_dir.clone(),
            manifests: Mutex::new(HashMap::new()),
            jobs,
            detectors,
            refiner,
            p2p: std::sync::OnceLock::new(),
        }))
    }
}

impl Server {
    /// Open the store + databases and bind the listen socket.
    ///
    /// # Errors
    /// Store/DB open failures, bind failures.
    pub fn bind(config: &Config) -> anyhow::Result<Self> {
        let app = App::open(config)?;
        // The M4 "NO AUTHENTICATION" warning died with D68: binding
        // wide now means "auth required", not "everyone is owner".
        if !config.listen.ip().is_loopback() {
            info!(
                "listening on non-loopback {}: auth required — non-loopback requests \
                 need a session or bearer token (mint invites with `datboi user invite`)",
                config.listen
            );
        }
        if let Some(addr) = config.nfs_listen
            && !addr.ip().is_loopback()
        {
            // NFS has no auth story yet (D68 keeps it loopback-only-by-
            // default in M5), so a wide NFS bind stays a loud warning.
            warn!(
                "NFS listening on non-loopback {addr} with NO AUTHENTICATION; \
                 anyone who can reach this socket can read every view"
            );
        }
        let listener = std::net::TcpListener::bind(config.listen)
            .with_context(|| format!("binding {}", config.listen))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            nfs_listen: config.nfs_listen,
            p2p: config.p2p,
            app,
        })
    }

    /// The bound address (useful when the config asked for port 0).
    ///
    /// # Errors
    /// Socket introspection failure.
    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Serve until SIGINT/SIGTERM. Blocking: builds its own tokio
    /// runtime — the CLI's client subcommands never enter async.
    ///
    /// # Errors
    /// Runtime construction or fatal accept-loop errors.
    pub fn serve(self) -> anyhow::Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building tokio runtime")?;
        runtime.block_on(async move {
            if let Some(addr) = self.nfs_listen {
                use nfsserve::tcp::NFSTcp as _;
                let fs = nfs::NfsFs::new(Arc::clone(&self.app));
                let nfs_listener = nfsserve::tcp::NFSTcpListener::bind(&addr.to_string(), fs)
                    .await
                    .with_context(|| format!("binding NFS on {addr}"))?;
                println!(
                    "datboi-server NFSv3 on {addr} \
                     (mount -o nolock,vers=3,tcp,port={port},mountport={port} <host>:/ <dir>)",
                    port = nfs_listener.get_listen_port()
                );
                tokio::spawn(async move {
                    if let Err(e) = nfs_listener.handle_forever().await {
                        error!("nfs listener died: {e}");
                    }
                });
            }
            // p2p seedbox (D97/D99): opt-in. Serves our holdings — the
            // whole logical CAS (D92) — to peers under the DERIVED iroh
            // identity. A start failure (offline, discovery down) is a
            // warning, never fatal: the daemon's local surfaces stand.
            let seedbox = if self.p2p {
                let identity =
                    datboi_catalog::statesnap::load_or_create_identity(&self.app.db_dir)?;
                // A dedicated read-only Db so serving reads never contend
                // with the request path's pools (serving is read-only).
                let p2p_db = Arc::new(Mutex::new(Db::open_read_only(&self.app.db_dir)?));
                match datboi_p2p::serve_holdings(self.app.store, p2p_db, identity.iroh_secret())
                    .await
                {
                    Ok(sb) => {
                        info!(
                            "p2p: serving holdings — iroh endpoint {} (share this as your peer id)",
                            sb.node_id()
                        );
                        // Outbound sync (D101) rides this same endpoint;
                        // set-once, and requests can't race it (axum
                        // hasn't started serving yet).
                        let _ = self.app.p2p.set(sb.client());
                        Some(sb)
                    }
                    Err(e) => {
                        warn!("p2p: seedbox failed to start ({e}); serving locally only");
                        None
                    }
                }
            } else {
                None
            };

            let listener = tokio::net::TcpListener::from_std(self.listener)?;
            let router = http::router(self.app);
            // ConnectInfo carries the peer address into the auth gate:
            // loopback-is-owner (D68) needs to know who's asking.
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await?;
            if let Some(sb) = seedbox
                && let Err(e) = sb.shutdown().await
            {
                warn!("p2p: seedbox shutdown: {e}");
            }
            Ok(())
        })
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

/// Bind and serve (the `datboi serve` entry point). Logs the bound
/// address once the socket is live (INFO — the default filter, so it
/// reaches the operator even without RUST_LOG).
///
/// # Errors
/// See [`Server::bind`] and [`Server::serve`].
pub fn run(config: &Config) -> anyhow::Result<()> {
    let server = Server::bind(config)?;
    info!(
        "datboi-server listening on http://{} (store {}, db {})",
        server.local_addr()?,
        config.store_root.display(),
        config.db_dir.display()
    );
    server.serve()
}
