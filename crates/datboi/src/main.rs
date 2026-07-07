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
    Serve,
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
    /// Re-hash stored blobs and report corruption.
    Scrub {
        /// Percentage of blobs to check (deterministic sample by hash).
        #[arg(long, default_value_t = 100)]
        sample: u8,
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

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Serve => {
            datboi_server::run()?;
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
        Command::Scrub { sample, json } => cmds::scrub(&cli.global.open()?, sample, json),
        Command::Status { json } => cmds::status(&cli.global.open()?, json),
    }
}
