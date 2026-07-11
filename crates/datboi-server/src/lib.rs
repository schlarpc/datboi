//! The datboi daemon (docs/50-infra.md): axum + tokio, 12-factor config
//! via env, serving view snapshots over HTTP with Range support (M4,
//! docs/80-views.md). Localhost-only by default — there is no auth
//! until M5, so binding beyond loopback is an explicit operator choice
//! that gets a loud warning.
//!
//! Serving surfaces present SNAPSHOTS only (D33): `/view/<name>/…`
//! resolves the `view/<name>` tag per request (so an eval flips the
//! tree atomically between requests, never mid-read — reads hold the
//! resolved snapshot), and `/snap/<hash>/…` addresses any snapshot
//! immutably.

mod dav;
mod http;
mod nfs;
mod vfs;
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
    /// bind — the same no-auth warning applies until M5.
    pub nfs_listen: Option<SocketAddr>,
}

/// Shared server state. One SQLite handle behind a mutex serializes
/// index reads and recipe execution across requests — correct first;
/// per-worker read connections are a measured-need optimization.
pub(crate) struct App {
    pub(crate) db: Mutex<Db>,
    pub(crate) exec: Executor<'static>,
    pub(crate) store: &'static Store,
    /// Decoded manifests by snapshot hash. Immutable objects, so
    /// entries never invalidate; bounded by wholesale clear.
    pub(crate) manifests: Mutex<HashMap<Blake3, Arc<vfs::ViewIndex>>>,
}

/// A bound-but-not-yet-serving daemon, so callers (and tests) can learn
/// the actual address before requests flow.
pub struct Server {
    listener: std::net::TcpListener,
    nfs_listen: Option<SocketAddr>,
    app: Arc<App>,
}

impl App {
    /// Open store + databases into shared daemon state.
    fn open(store_root: &std::path::Path, db_dir: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let store = Store::open(store_root)
            .with_context(|| format!("opening store at {}", store_root.display()))?;
        // The executor borrows the store for its lifetime; the daemon's
        // lifetime IS the process lifetime, so one leaked Store is the
        // honest expression of that (no self-referential gymnastics).
        let store: &'static Store = Box::leak(Box::new(store));
        let db = Db::open(db_dir)
            .with_context(|| format!("opening databases in {}", db_dir.display()))?;
        let exec = Executor::new(store, ExecConfig::default())?;
        Ok(Arc::new(App {
            db: Mutex::new(db),
            exec,
            store,
            manifests: Mutex::new(HashMap::new()),
        }))
    }
}

impl Server {
    /// Open the store + databases and bind the listen socket.
    ///
    /// # Errors
    /// Store/DB open failures, bind failures.
    pub fn bind(config: &Config) -> anyhow::Result<Self> {
        let app = App::open(&config.store_root, &config.db_dir)?;
        for addr in std::iter::once(config.listen).chain(config.nfs_listen) {
            if !addr.ip().is_loopback() {
                eprintln!(
                    "warning: listening on non-loopback {addr} with NO AUTHENTICATION \
                     (auth is M5); anyone who can reach this socket can read every view"
                );
            }
        }
        let listener = std::net::TcpListener::bind(config.listen)
            .with_context(|| format!("binding {}", config.listen))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            nfs_listen: config.nfs_listen,
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
                        eprintln!("nfs listener died: {e}");
                    }
                });
            }
            let listener = tokio::net::TcpListener::from_std(self.listener)?;
            let router = http::router(self.app);
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
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
/// address to stdout once the socket is live.
///
/// # Errors
/// See [`Server::bind`] and [`Server::serve`].
pub fn run(config: &Config) -> anyhow::Result<()> {
    let server = Server::bind(config)?;
    println!(
        "datboi-server listening on http://{} (store {}, db {})",
        server.local_addr()?,
        config.store_root.display(),
        config.db_dir.display()
    );
    server.serve()
}
