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

mod http;
mod vfs;

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
    /// Listen address; loopback unless the operator opted out.
    pub listen: SocketAddr,
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
    app: Arc<App>,
}

impl Server {
    /// Open the store + databases and bind the listen socket.
    ///
    /// # Errors
    /// Store/DB open failures, bind failures.
    pub fn bind(config: &Config) -> anyhow::Result<Self> {
        let store = Store::open(&config.store_root)
            .with_context(|| format!("opening store at {}", config.store_root.display()))?;
        // The executor borrows the store for its lifetime; the daemon's
        // lifetime IS the process lifetime, so one leaked Store is the
        // honest expression of that (no self-referential gymnastics).
        let store: &'static Store = Box::leak(Box::new(store));
        let db = Db::open(&config.db_dir)
            .with_context(|| format!("opening databases in {}", config.db_dir.display()))?;
        let exec = Executor::new(store, ExecConfig::default())?;
        if !config.listen.ip().is_loopback() {
            eprintln!(
                "warning: listening on non-loopback {} with NO AUTHENTICATION (auth is M5); \
                 anyone who can reach this socket can read every view",
                config.listen
            );
        }
        let listener = std::net::TcpListener::bind(config.listen)
            .with_context(|| format!("binding {}", config.listen))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            app: Arc::new(App {
                db: Mutex::new(db),
                exec,
                store,
                manifests: Mutex::new(HashMap::new()),
            }),
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
