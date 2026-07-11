//! Crash-injection points for the crash-consistency harness.
//!
//! These are compiled to inlined no-ops unless the `crash-injection`
//! feature is enabled, so a shipping binary can never abort here. With the
//! feature on, an injection point aborts the process (SIGABRT — no
//! unwinding, no destructors, no flush) when `DATBOI_CRASH_PHASE` names it,
//! modelling a `kill -9` at that exact step of the publish protocol. The
//! `datboi-crash-child` test binary drives this; see `tests/crash_harness.rs`.

/// A step in [`crate::store::Store::put_new`]'s publish protocol at which a
/// crash can be injected — named for the step that just completed (mid-write
/// is separate: it carries a byte count). The next step has NOT run yet.
#[derive(Clone, Copy)]
pub(crate) enum Phase {
    TempCreated,
    Written,
    Fsynced,
    Renamed,
}

#[cfg(feature = "crash-injection")]
mod imp {
    use super::Phase;

    impl Phase {
        fn env_name(self) -> &'static str {
            match self {
                Phase::TempCreated => "after-temp-create",
                Phase::Written => "after-write",
                Phase::Fsynced => "after-fsync",
                Phase::Renamed => "after-rename",
            }
        }
    }

    fn selected_phase() -> Option<String> {
        std::env::var("DATBOI_CRASH_PHASE").ok()
    }

    pub(crate) fn inject(phase: Phase) {
        if selected_phase().as_deref() == Some(phase.env_name()) {
            std::process::abort();
        }
    }

    pub(crate) fn inject_mid_write(written: u64) {
        if selected_phase().as_deref() == Some("mid-write") {
            let at: u64 = std::env::var("DATBOI_CRASH_AT_BYTES")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if written >= at {
                std::process::abort();
            }
        }
    }
}

#[cfg(not(feature = "crash-injection"))]
mod imp {
    use super::Phase;

    #[inline(always)]
    pub(crate) fn inject(_phase: Phase) {}

    #[inline(always)]
    pub(crate) fn inject_mid_write(_written: u64) {}
}

pub(crate) use imp::{inject, inject_mid_write};
