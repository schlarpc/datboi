//! Daemon entry point. 12-factor: config via env, structured logs to
//! stdout, no pidfiles (docs/50-infra.md). Localhost/unix-socket only
//! until M4 (D35).

/// Run the daemon until shutdown.
///
/// # Errors
/// Currently always: not yet implemented (M1 critical path).
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("datboi-server is not implemented yet — see docs/90-roadmap.md (M1)")
}
