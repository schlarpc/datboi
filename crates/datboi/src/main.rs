//! The datboi CLI: client subcommands plus `serve` (docs/85-cli.md draft).

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "dat/rom management on content-addressed storage")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon (12-factor; config via env / DATBOI_* variables).
    Serve,
    /// Hash and claim content from a directory into the store.
    Ingest {
        /// Directory (or file) to ingest.
        path: std::path::PathBuf,
    },
    /// Report have/missing/unknown against an imported dat.
    Audit {
        /// Dat source to audit against (provider/system).
        dat: String,
    },
    /// Rebuild local databases from the store (bare-NAS recovery, D15).
    Recover,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => datboi_server::run(),
        Command::Ingest { .. } | Command::Audit { .. } | Command::Recover => {
            anyhow::bail!("not implemented yet — see docs/90-roadmap.md (M1)")
        }
    }
}
