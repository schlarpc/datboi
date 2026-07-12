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
        /// Listen address. Loopback by default; loopback connections
        /// are implicitly owner, wider binds require auth (D68).
        #[arg(
            long,
            env = "DATBOI_LISTEN",
            default_value = "127.0.0.1:2352",
            value_name = "ADDR"
        )]
        listen: std::net::SocketAddr,
        /// Also serve NFSv3 on this address (off unless set). Consoles
        /// need a LAN bind; NFS carries NO auth (loopback-only-by-
        /// default in M5, D68).
        #[arg(long, env = "DATBOI_NFS_LISTEN", value_name = "ADDR")]
        nfs_listen: Option<std::net::SocketAddr>,
        /// Disable ambient refinement (D71): no background analyzer
        /// worker; sweeps go back to being a manual `datboi sweep`
        /// errand. Per-family control stays `datboi analyzer`.
        #[arg(long, env = "DATBOI_NO_REFINE")]
        no_refine: bool,
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
    /// Orphan review + GC policy (D72/D73): list reviewable orphan
    /// candidates, keep-mark, apply deletions, tune watermarks/grace.
    Gc {
        #[command(subcommand)]
        cmd: GcCommand,
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
    /// Local accounts, invites, and view grants (auth v1, D30/D68).
    /// Minting stays in the CLI: local shell access = admin.
    User {
        #[command(subcommand)]
        cmd: UserCommand,
    },
    /// Mint a session token for a user and print it once (for remote
    /// tools via `Authorization: Bearer`; loopback is already owner).
    Token {
        /// Username the token acts as.
        #[arg(long)]
        user: String,
        /// Token lifetime in days.
        #[arg(long, default_value_t = 30)]
        expires_days: u32,
        #[arg(long)]
        json: bool,
    },
    /// Session administration (revocation is per-user, D68).
    Session {
        #[command(subcommand)]
        cmd: SessionCommand,
    },
}

#[derive(Subcommand)]
enum UserCommand {
    /// Mint a single-use invite URL (D68). The token rides in the URL
    /// fragment so it never appears in server logs.
    Invite {
        /// Mint an owner invite instead of the default friend.
        #[arg(long)]
        owner: bool,
        /// Invite lifetime in days.
        #[arg(long, default_value_t = 7)]
        expires_days: u32,
        /// URL prefix for the printed invite (e.g. http://nas.local:2352).
        /// Defaults to http://<DATBOI_LISTEN or 127.0.0.1:2352>.
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List users with roles and grant counts.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Grant a user access to a view (friends see exactly their grants).
    Grant {
        username: String,
        view: String,
        #[arg(long)]
        json: bool,
    },
    /// Revoke a user's access to a view.
    Revoke {
        username: String,
        view: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// List live sessions (username + expiry).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Revoke ALL of a user's sessions (cookies and bearer tokens).
    Revoke {
        username: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum GcCommand {
    /// Reviewable orphan candidates (past grace, unrooted, with ingest
    /// provenance). Marks are review state — apply re-verifies.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Keep-mark a hash ("this is not junk"): excluded from apply,
    /// survives cache rebuilds.
    Keep {
        /// Blob hash (blake3 hex).
        hash: String,
    },
    /// Clear a keep-mark.
    Unkeep {
        /// Blob hash (blake3 hex).
        hash: String,
    },
    /// Delete reviewed candidates (D73's one destructive action). Every
    /// deletion re-verifies unreferenced + aged + unkept at delete time
    /// under the D72 guard.
    Apply {
        /// Specific hashes; none = every reviewable, non-kept candidate.
        hashes: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show or set GC policy (D72 watermarks, D73 grace). Values:
    /// "off", "NN%", or absolute bytes.
    Config {
        #[arg(long)]
        high_water: Option<String>,
        #[arg(long)]
        low_water: Option<String>,
        #[arg(long)]
        grace_secs: Option<i64>,
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
    /// Link a retool clonelist to a dat source (D57): refines 1G1R
    /// clone families in both held-first and strict modes.
    Clonelist {
        /// Source as <provider>/<system>.
        source: String,
        /// Retool clonelist JSON file.
        file: PathBuf,
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
        /// D57 strict 1G1R: pick purely from (dat, preferences) — the
        /// preferred entry wins even when absent (its gaps ARE the
        /// want list). Default is held-first: held copies outrank
        /// preferred-but-absent ones.
        #[arg(long, requires = "one_per_family")]
        strict: bool,
        /// Render a MAME listxml source as merge-mode sets:
        /// non-merged (standalone sets, device_ref closure), split,
        /// or merged (clones fold into parents). Exclusive with --1g1r.
        #[arg(long, conflicts_with = "one_per_family")]
        mame_mode: Option<String>,
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

/// D74's structurally-can't-forget device: this match is EXHAUSTIVE
/// ON PURPOSE — no wildcard arm, ever. Adding a `Command` variant
/// refuses to compile until its author answers, HERE, whether it is a
/// byte-level job the ledger records (`Some(kind, name)`) or not
/// (`None`). The compiler asks the question so no reviewer has to
/// remember it. CLI rows are TERMINAL-ONLY (insert_finished_job): a
/// `running` row from a CLI process would be falsely tombstoned by
/// the daemon's interruption sweep, whose one-daemon-per-db-dir
/// assumption a live CLI legitimately violates.
fn ledger_stamp(command: &Command) -> Option<(i64, String)> {
    use datboi_index::jobs::{KIND_GC, KIND_INGEST, KIND_REFINE, KIND_SCRUB};
    match command {
        // ---- byte-level jobs: recorded ----
        Command::Ingest { paths, .. } => Some((
            KIND_INGEST,
            format!(
                "cli: ingest — {} path{}",
                paths.len(),
                if paths.len() == 1 { "" } else { "s" }
            ),
        )),
        Command::Evict { dry_run: false, .. } => Some((KIND_GC, "cli: evict".into())),
        Command::Materialize { hash, .. } => Some((
            KIND_GC,
            format!("cli: materialize {}", &hash[..hash.len().min(10)]),
        )),
        Command::Sweep { analyzer, .. } => {
            Some((KIND_REFINE, format!("cli: sweep — {analyzer}")))
        }
        Command::Gc {
            cmd: GcCommand::Apply { .. },
        } => Some((KIND_GC, "cli: gc apply".into())),
        Command::Scrub { sample, .. } => {
            Some((KIND_SCRUB, format!("cli: scrub — {sample}% sample")))
        }

        // ---- not jobs: reads, config, auth, serving ----
        // Dry-run evict plans; gc list/keep/config mutate policy rows,
        // not bytes.
        Command::Evict { dry_run: true, .. } | Command::Gc { .. } => None,
        // The daemon records its own jobs through the registry.
        Command::Serve { .. } => None,
        // Catalog/config/read surfaces. View Eval/Image and
        // Recover/Snapshot are real byte-level work that deserve their
        // OWN kinds when their history surfaces exist (the
        // open-questions eval-report entry) — do not shoehorn them
        // into Gc.
        Command::Dat(_)
        | Command::Audit { .. }
        | Command::Export(_)
        | Command::Recover { .. }
        | Command::Snapshot { .. }
        | Command::Analyzer { .. }
        | Command::View { .. }
        | Command::Status { .. }
        | Command::User { .. }
        | Command::Token { .. }
        | Command::Session { .. } => None,
    }
}

/// Best-effort terminal ledger row for a stamped CLI command (D74):
/// history failing to write must never fail the work it describes.
fn record_cli_job(db_dir: &std::path::Path, kind: i64, name: &str, started: i64, failed: bool) {
    use datboi_index::jobs::{JOB_DONE, JOB_FAILED, LEDGER_KEEP};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    match datboi_index::Db::open(db_dir) {
        Ok(db) => {
            let state = if failed { JOB_FAILED } else { JOB_DONE };
            if let Err(e) = db.insert_finished_job(kind, name, state, started, now) {
                eprintln!("warning: job ledger write failed: {e}");
            }
            let _ = db.prune_jobs(LEDGER_KEEP);
        }
        Err(e) => eprintln!("warning: job ledger unavailable: {e}"),
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    // Stamp BEFORE dispatch (the arms consume `cli`); record after.
    let stamp = ledger_stamp(&cli.command);
    let ledger_dir = stamp
        .is_some()
        .then(|| cli.global.resolve_db_dir().ok())
        .flatten();
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let result = dispatch(cli);
    if let (Some((kind, name)), Some(dir)) = (stamp, ledger_dir) {
        record_cli_job(&dir, kind, &name, started, result.is_err());
    }
    result
}

fn dispatch(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Serve {
            listen,
            nfs_listen,
            no_refine,
        } => {
            let Some(store_root) = cli.global.store.clone() else {
                anyhow::bail!("store root not set: pass --store or set DATBOI_STORE");
            };
            let db_dir = cli.global.resolve_db_dir()?;
            datboi_server::run(&datboi_server::Config {
                store_root,
                db_dir,
                listen,
                nfs_listen,
                // The global --detectors/DATBOI_DETECTORS flag: web
                // ingest applies the same skipper set CLI ingest does.
                detectors_dir: cli.global.detectors.clone(),
                refine: !no_refine,
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
        Command::Dat(DatCommand::Clonelist { source, file, json }) => {
            cmds::dat_clonelist(&cli.global.open()?, &source, &file, json)
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
        Command::Gc { cmd } => match cmd {
            GcCommand::List { json } => cmds::gc_list(&cli.global.open()?, json),
            GcCommand::Keep { hash } => cmds::gc_keep(&cli.global.open()?, &hash, true),
            GcCommand::Unkeep { hash } => cmds::gc_keep(&cli.global.open()?, &hash, false),
            GcCommand::Apply { hashes, json } => cmds::gc_apply(cli.global.open()?, &hashes, json),
            GcCommand::Config {
                high_water,
                low_water,
                grace_secs,
                json,
            } => cmds::gc_config(
                &cli.global.open()?,
                high_water.as_deref(),
                low_water.as_deref(),
                grace_secs,
                json,
            ),
        },
        Command::View { cmd } => match cmd {
            ViewCommand::Define {
                name,
                source,
                template,
                one_per_family,
                regions,
                langs,
                strict,
                mame_mode,
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
                one_per_family.then_some(datboi_catalog::SelectionPolicy {
                    regions,
                    langs,
                    strict,
                }),
                mame_mode
                    .as_deref()
                    .map(datboi_catalog::MameMode::parse)
                    .transpose()?,
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
        Command::User { cmd } => match cmd {
            UserCommand::Invite {
                owner,
                expires_days,
                base_url,
                json,
            } => cmds::user_invite(
                &cli.global.open()?,
                owner,
                expires_days,
                base_url.as_deref(),
                json,
            ),
            UserCommand::List { json } => cmds::user_list(&cli.global.open()?, json),
            UserCommand::Grant {
                username,
                view,
                json,
            } => cmds::user_grant(&cli.global.open()?, &username, &view, true, json),
            UserCommand::Revoke {
                username,
                view,
                json,
            } => cmds::user_grant(&cli.global.open()?, &username, &view, false, json),
        },
        Command::Token {
            user,
            expires_days,
            json,
        } => cmds::token_mint(&cli.global.open()?, &user, expires_days, json),
        Command::Session { cmd } => match cmd {
            SessionCommand::List { json } => cmds::session_list(&cli.global.open()?, json),
            SessionCommand::Revoke { username, json } => {
                cmds::session_revoke(&cli.global.open()?, &username, json)
            }
        },
    }
}
