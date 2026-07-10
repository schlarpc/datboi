//! The datboi CLI: client subcommands plus `serve` (docs/85-cli.md).
//!
//! Exit codes: 0 success/complete, 1 incomplete or problems found
//! (audit/scrub/ingest-with-errors), 2 usage or runtime error.

mod cmds;
mod config;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::config::GlobalArgs;

#[derive(Parser)]
#[command(version, about = "dat/rom management on content-addressed storage")]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon (12-factor; config via env / DATBOI_* variables).
    Serve {
        /// Listen address. Loopback by default: there is no auth until
        /// M5, so a wider bind is an explicit operator choice.
        #[arg(
            long,
            env = "DATBOI_LISTEN",
            default_value = "127.0.0.1:2352",
            value_name = "ADDR"
        )]
        listen: std::net::SocketAddr,
    },
    /// Hash and claim content into the store (copy semantics, D40).
    Ingest {
        /// Files or directories to ingest.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Rename sources into the store instead of copying (destroys the
        /// source layout — that's the point). Not implemented yet.
        #[arg(long = "move")]
        mv: bool,
        #[arg(long)]
        json: bool,
    },
    /// Dat operations.
    #[command(subcommand)]
    Dat(DatCommand),
    /// Report have/missing against an imported dat source.
    Audit {
        /// Source as <provider>/<system> (see `datboi dat list`).
        source: String,
        /// Only list entries with missing roms.
        #[arg(long)]
        missing: bool,
        #[arg(long)]
        json: bool,
    },
    /// Export operations.
    #[command(subcommand)]
    Export(ExportCommand),
    /// Rebuild local databases from the store (D15): blob and recipe
    /// indexes from a full read pass, catalog state replayed from the
    /// newest state snapshot that verifies under this instance's identity.
    Recover {
        #[arg(long)]
        json: bool,
    },
    /// Mint a signed state snapshot into the store (the recovery root).
    Snapshot {
        #[arg(long)]
        json: bool,
    },
    /// Evict recipe-covered literals to reclaim space (D25/D27): only
    /// blobs reconstructible through locally-replayed recipes grounded in
    /// retained literals are dropped; outboards are kept so evicted
    /// content still serves verified range reads (D49).
    Evict {
        /// Resident data bytes to keep (0 = evict everything evictable).
        #[arg(long, default_value_t = 0)]
        target_bytes: u64,
        /// Replay not-yet-licensed recipe routes first (CPU for bytes).
        #[arg(long)]
        license: bool,
        /// Report what would be evicted without deleting anything.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Rematerialize an evicted or claimed blob into the store by
    /// replaying its cheapest recipe route.
    Materialize {
        /// Blob hash (blake3 hex).
        hash: String,
        #[arg(long)]
        json: bool,
    },
    /// Run one background refinement sweep round (D45): analyze stored
    /// blobs with the named analyzer, recording provenance (including
    /// negative results) for the fixpoint.
    Sweep {
        /// Analyzer name (currently: noop).
        #[arg(long, default_value = "noop")]
        analyzer: String,
        /// Maximum items to analyze this round.
        #[arg(long, default_value_t = 10_000)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Filesystem views (M4, 80-views.md): define policies, evaluate
    /// them into immutable snapshots, list what exists.
    View {
        #[command(subcommand)]
        cmd: ViewCommand,
    },
    /// Re-hash stored blobs and report corruption.
    Scrub {
        /// Percentage of blobs to check (deterministic sample by hash).
        #[arg(long, default_value_t = 100)]
        sample: u8,
        /// Re-execute poisoned (Failed) recipes; a verified re-replay
        /// clears the poison — the escape hatch for wrong poisonings.
        #[arg(long)]
        rehabilitate: bool,
        #[arg(long)]
        json: bool,
    },
    /// Store and database overview.
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DatCommand {
    /// Import a dat file (manual drop flow, D16).
    Import {
        file: PathBuf,
        /// Provider label (defaults derive from the dat header).
        #[arg(long)]
        provider: Option<String>,
        /// System label (defaults derive from the dat header).
        #[arg(long)]
        system: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List imported sources and their current revisions.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Fetch a dat over HTTP and import it (Redump auto-fetch, D16).
    Fetch {
        /// Full URL, or redump/<system-slug> (e.g. redump/psx).
        source: String,
        /// Provider label (redump/... defaults to "Redump").
        #[arg(long)]
        provider: Option<String>,
        /// System label (defaults derive from the dat header).
        #[arg(long)]
        system: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Diff a source's two newest revisions (previous → current, D38).
    /// Exit code: 0 no changes, 1 changes, 2 error.
    Diff {
        /// Source as <provider>/<system>.
        source: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ExportCommand {
    /// Export a source's current revision as a Logiqx dat (dir2dat, D29).
    Dat {
        /// Source as <provider>/<system>.
        source: String,
        /// Output file.
        #[arg(short, long)]
        out: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}

#[derive(clap::Subcommand)]
enum ViewCommand {
    /// Define (or replace) a named view over one dat source.
    Define {
        name: String,
        /// <provider>/<system> of the dat source to project.
        source: String,
        /// Layout template; placeholders {entry} and {name}.
        #[arg(long, default_value = "{name}")]
        template: String,
        #[arg(long)]
        json: bool,
    },
    /// Evaluate a view into an immutable snapshot and flip its tag (D33).
    Eval {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// List defined views and their current snapshots.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print a view's current manifest.
    Manifest {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Serve { listen } => {
            let Some(store_root) = cli.global.store.clone() else {
                anyhow::bail!("store root not set: pass --store or set DATBOI_STORE");
            };
            let Some(db_dir) = cli.global.db_dir.clone() else {
                anyhow::bail!(
                    "database dir not set: pass --db-dir or set DATBOI_DB_DIR (local disk, not NFS)"
                );
            };
            datboi_server::run(&datboi_server::Config {
                store_root,
                db_dir,
                listen,
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Ingest { paths, mv, json } => cmds::ingest(cli.global.open()?, &paths, mv, json),
        Command::Dat(DatCommand::Import {
            file,
            provider,
            system,
            json,
        }) => cmds::dat_import(
            cli.global.open()?,
            &file,
            provider.as_deref(),
            system.as_deref(),
            json,
        ),
        Command::Dat(DatCommand::List { json }) => cmds::dat_list(&cli.global.open()?, json),
        Command::Dat(DatCommand::Fetch {
            source,
            provider,
            system,
            json,
        }) => cmds::dat_fetch(
            cli.global.open()?,
            &source,
            provider.as_deref(),
            system.as_deref(),
            json,
        ),
        Command::Dat(DatCommand::Diff { source, json }) => {
            cmds::dat_diff(&cli.global.open()?, &source, json)
        }
        Command::Audit {
            source,
            missing,
            json,
        } => cmds::audit_cmd(&cli.global.open()?, &source, missing, json),
        Command::Export(ExportCommand::Dat { source, out }) => {
            cmds::export_dat_cmd(&cli.global.open()?, &source, &out)
        }
        Command::Recover { json } => cmds::recover(cli.global.open()?, json),
        Command::Snapshot { json } => cmds::snapshot(cli.global.open()?, json),
        Command::Evict {
            target_bytes,
            license,
            dry_run,
            json,
        } => cmds::evict(cli.global.open()?, target_bytes, license, dry_run, json),
        Command::Materialize { hash, json } => cmds::materialize(cli.global.open()?, &hash, json),
        Command::View { cmd } => match cmd {
            ViewCommand::Define {
                name,
                source,
                template,
                json,
            } => cmds::view_define(&cli.global.open()?, &name, &source, &template, json),
            ViewCommand::Eval { name, json } => cmds::view_eval(cli.global.open()?, &name, json),
            ViewCommand::List { json } => cmds::view_list(&cli.global.open()?, json),
            ViewCommand::Manifest { name, json } => {
                cmds::view_manifest(&cli.global.open()?, &name, json)
            }
        },
        Command::Sweep {
            analyzer,
            limit,
            json,
        } => cmds::sweep(cli.global.open()?, &analyzer, limit, json),
        Command::Scrub {
            sample,
            rehabilitate,
            json,
        } => cmds::scrub(&cli.global.open()?, sample, rehabilitate, json),
        Command::Status { json } => cmds::status(&cli.global.open()?, json),
    }
}
