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
        /// Also serve NFSv3 on this address (off unless set). Consoles
        /// need a LAN bind; the same no-auth caveat applies.
        #[arg(long, env = "DATBOI_NFS_LISTEN", value_name = "ADDR")]
        nfs_listen: Option<std::net::SocketAddr>,
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
    /// Ingest-policy config for background analyzers (D60): per-family
    /// enable/disable + analyzer-owned opaque params in the config KV.
    Analyzer {
        #[command(subcommand)]
        cmd: AnalyzerCommand,
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
enum AnalyzerCommand {
    /// List analyzer families with enable state and params presence.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Enable a family (the default state).
    Enable { name: String },
    /// Disable a family: its sweeps become no-ops until re-enabled.
    Disable { name: String },
    /// Set a family's opaque params (hex bytes; the analyzer owns the
    /// encoding — nothing else interprets them).
    SetParams { name: String, hex: String },
    /// Clear a family's params.
    ClearParams { name: String },
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
        /// Keep one entry per clone family (1G1R) instead of everything.
        #[arg(long = "1g1r")]
        one_per_family: bool,
        /// Ordered region priority for 1G1R, comma-separated
        /// (e.g. "USA,Europe,Japan"; aliases US/EU/JP accepted).
        #[arg(long, value_delimiter = ',', requires = "one_per_family")]
        regions: Vec<String>,
        /// Ordered language priority for 1G1R (e.g. "En").
        #[arg(long, value_delimiter = ',', requires = "one_per_family")]
        langs: Vec<String>,
        /// Constraint profile for the target device (see `view profiles`).
        #[arg(long)]
        profile: Option<String>,
        /// Also reify the view as a FAT32 image (D62); mint with
        /// `view image <name>` after evaluating.
        #[arg(long)]
        image: bool,
        /// Image cluster size in bytes (power of two, 512..=65536).
        #[arg(long, default_value_t = 32 * 1024, requires = "image")]
        image_cluster_size: u32,
        /// Bare filesystem (superfloppy) instead of an MBR partition table.
        #[arg(long, requires = "image")]
        image_no_partition: bool,
        /// Volume label (defaults to the view name).
        #[arg(long, requires = "image")]
        image_label: Option<String>,
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
    /// List built-in constraint profiles.
    Profiles {
        #[arg(long)]
        json: bool,
    },
    /// Materialize a view's current snapshot into a directory (SD sync
    /// for flashcarts). Incremental by size; bytes flow through the
    /// verified range path.
    Sync {
        name: String,
        /// Target directory (e.g. a mounted SD card).
        target: PathBuf,
        /// Remove files not in the snapshot (and newly-empty dirs).
        #[arg(long)]
        delete: bool,
        /// Re-hash size-matched files instead of trusting size alone.
        #[arg(long)]
        verify: bool,
        /// Report what would change without touching the target.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Mint the view's FAT32 image recipe (D62) from its current
    /// snapshot and flip the `image/<name>` tag; optionally export the
    /// bytes. NOTE: reflashing an exported image CLOBBERS on-device
    /// saves (writable overlays are a future design pass).
    Image {
        name: String,
        /// Write the image bytes to this file (temp + fsync + rename).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Skip storing the output obao sidecar; serving then relies on
        /// the D63 affine carve-out.
        #[arg(long)]
        no_obao: bool,
        #[arg(long)]
        json: bool,
    },
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Serve { listen, nfs_listen } => {
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
                nfs_listen,
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
                one_per_family,
                regions,
                langs,
                profile,
                image,
                image_cluster_size,
                image_no_partition,
                image_label,
                json,
            } => cmds::view_define(
                &cli.global.open()?,
                &name,
                &source,
                &template,
                one_per_family.then_some(datboi_catalog::SelectionPolicy { regions, langs }),
                profile,
                image.then_some(datboi_catalog::ImageParams {
                    cluster_size: image_cluster_size,
                    partition: !image_no_partition,
                    label: image_label,
                }),
                json,
            ),
            ViewCommand::Eval { name, json } => cmds::view_eval(cli.global.open()?, &name, json),
            ViewCommand::List { json } => cmds::view_list(&cli.global.open()?, json),
            ViewCommand::Profiles { json } => cmds::view_profiles(json),
            ViewCommand::Sync {
                name,
                target,
                delete,
                verify,
                dry_run,
                json,
            } => cmds::view_sync(
                &cli.global.open()?,
                &name,
                &target,
                delete,
                verify,
                dry_run,
                json,
            ),
            ViewCommand::Manifest { name, json } => {
                cmds::view_manifest(&cli.global.open()?, &name, json)
            }
            ViewCommand::Image {
                name,
                out,
                no_obao,
                json,
            } => cmds::view_image(cli.global.open()?, &name, out.as_deref(), no_obao, json),
        },
        Command::Sweep {
            analyzer,
            limit,
            json,
        } => cmds::sweep(cli.global.open()?, &analyzer, limit, json),
        Command::Analyzer { cmd } => match cmd {
            AnalyzerCommand::List { json } => cmds::analyzer_list(&cli.global.open()?, json),
            AnalyzerCommand::Enable { name } => {
                cmds::analyzer_set_enabled(&cli.global.open()?, &name, true)
            }
            AnalyzerCommand::Disable { name } => {
                cmds::analyzer_set_enabled(&cli.global.open()?, &name, false)
            }
            AnalyzerCommand::SetParams { name, hex } => {
                cmds::analyzer_set_params(&cli.global.open()?, &name, Some(&hex))
            }
            AnalyzerCommand::ClearParams { name } => {
                cmds::analyzer_set_params(&cli.global.open()?, &name, None)
            }
        },
        Command::Scrub {
            sample,
            rehabilitate,
            json,
        } => cmds::scrub(&cli.global.open()?, sample, rehabilitate, json),
        Command::Status { json } => cmds::status(&cli.global.open()?, json),
    }
}
