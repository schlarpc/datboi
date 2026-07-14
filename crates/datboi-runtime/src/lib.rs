//! wasmtime host for content-addressed transforms (docs/runtime.md,
//! docs/worlds.md, decisions D5–D7, D89).
//!
//! This crate runs components targeting the datboi lanes —
//! `datboi:transform@1` ([`stream`]) and `datboi:extractor@1`
//! ([`extractor`]), both importing `datboi:streams@1` — and its one job
//! beyond "call the component" is to make execution **deterministic and
//! bounded**, because storage recipes must replay bit-exact forever (D5)
//! and peer-supplied components are untrusted (their only threat is
//! resource abuse — the CAS verifies output bytes, D4).
//!
//! Lane majors are append-only (D89): when a lane's shape changes, the
//! new major gets a new host module and THIS one lives forever — never
//! "clean up" an old linker, old components must replay.

use thiserror::Error;
use wasmtime::Config;

pub mod attribution;
pub mod extractor;
pub mod pipe;
pub mod stream;

/// The deterministic engine configuration (D5) shared by every lane
/// host. Every knob here is part of the determinism contract.
#[must_use]
pub(crate) fn deterministic_config() -> Config {
    let mut config = Config::new();
    // Component model is the transform packaging format (D7).
    config.wasm_component_model(true);
    // NaN bit patterns are the one spec-sanctioned source of float
    // nondeterminism; canonicalize them so two hosts agree.
    config.cranelift_nan_canonicalization(true);
    // Threads need no knob: our wasmtime build omits the `threads` cargo
    // feature entirely — disabled at compile time.
    // Relaxed SIMD is *defined* to give implementation-specific results;
    // forbid it outright (plain SIMD stays on and is deterministic).
    config.wasm_relaxed_simd(false);
    // Meter fuel so runaway components trap at a fixed, reproducible point.
    config.consume_fuel(true);
    config
}

/// Seekability class a transform declares (docs/views.md, D27). Wire
/// values live in the descriptor CBOR schema (D89) — encoder in
/// `datboi-guest-transform`, decoder here, one schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekClass {
    /// Output ranges map to input ranges arithmetically.
    Affine,
    /// Random access via a content-addressed index (frame/block tables).
    ManifestSeekable,
    /// Whole-stream only; range reads require materialization first.
    Opaque,
}

/// A transform's static, pure capability metadata — the decoded form of
/// the canonical-CBOR bytes `describe` returns (D89 vocabulary rule:
/// `{1: seek, 2: random-access-inputs}`, key 2 omitted when empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub seek: SeekClass,
    pub random_access_inputs: Vec<u32>,
}

impl Descriptor {
    /// Decode a descriptor. Unknown keys are IGNORED — the D89 advisory
    /// rule: newer components run under older cores (D64), so added
    /// keys must never be load-bearing; anything a host must understand
    /// is a lane version. Known keys are still strictly checked (the
    /// house canonical decoder rejects non-canonical bytes outright).
    ///
    /// # Errors
    /// On non-canonical CBOR, a missing/invalid seek class, or a
    /// present-but-empty input list (one-encoding-per-value).
    pub fn from_cbor(bytes: &[u8]) -> Result<Self, String> {
        use datboi_core::cbor::{self, Value};
        let Ok(Value::Map(entries)) = cbor::decode(bytes) else {
            return Err("descriptor is not a canonical CBOR map".into());
        };
        let mut seek = None;
        let mut random_access_inputs = Vec::new();
        for (key, value) in entries {
            match key {
                1 => {
                    seek = Some(match value {
                        Value::Uint(0) => SeekClass::Affine,
                        Value::Uint(1) => SeekClass::ManifestSeekable,
                        Value::Uint(2) => SeekClass::Opaque,
                        other => return Err(format!("unknown seek class {other:?}")),
                    });
                }
                2 => {
                    let Value::Array(items) = value else {
                        return Err("random-access-inputs is not an array".into());
                    };
                    if items.is_empty() {
                        return Err("empty random-access-inputs must be omitted".into());
                    }
                    for item in items {
                        let Value::Uint(ix) = item else {
                            return Err("random-access-inputs entry is not a uint".into());
                        };
                        random_access_inputs.push(
                            u32::try_from(ix)
                                .map_err(|_| "random-access input index out of range")?,
                        );
                    }
                }
                // Advisory keys from a newer component: ignored (D89).
                _ => {}
            }
        }
        Ok(Self {
            seek: seek.ok_or("descriptor missing seek class")?,
            random_access_inputs,
        })
    }
}

/// Resource ceilings for a single transform run. Defaults are generous
/// enough for real codecs yet finite, so a hostile component fails instead
/// of hanging or OOMing the daemon (D5 sandboxing note).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Linear-memory ceiling in bytes.
    pub memory: usize,
    /// Fuel units; execution traps deterministically when exhausted. Fuel is
    /// a pure function of the code path (D5), so this bound never makes a
    /// *valid* run nondeterministic — it only kills runaway ones.
    pub fuel: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory: 1 << 30, // 1 GiB
            fuel: 10_000_000_000,
        }
    }
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("invalid transform component: {0}")]
    Component(#[source] wasmtime::Error),
    /// Instantiation/link failure: a host linker regression or a
    /// component/world mismatch. Wiring, not evidence about the guest's
    /// behavior — kept distinct from [`RuntimeError::Trap`] so the
    /// executor never poisons a claim over it.
    #[error("component instantiation failed (host/world wiring): {0}")]
    Instantiate(#[source] wasmtime::Error),
    #[error("transform trapped or exhausted its resource budget: {0}")]
    Trap(#[source] wasmtime::Error),
    #[error("transform returned an error: {0}")]
    Transform(String),
    /// A sequential input's reader ended before its declared length —
    /// the length CLAIM (an op-child's output claim, in the executor)
    /// is disproven, not the recipe being run. Carries attribution so
    /// the executor can refuse without poisoning the parent.
    #[error(
        "sequential input {input_ix} ended at {actual} bytes; its declared length is {claimed}"
    )]
    InputLengthMismatch {
        input_ix: u32,
        claimed: u64,
        actual: u64,
    },
}

impl RuntimeError {
    /// True when the failure is fuel exhaustion — a *policy* outcome
    /// (the budget was too small), not evidence against the claim. Fuel
    /// budgets may be retuned; a recipe must not be poisoned for one.
    #[must_use]
    pub fn is_fuel_exhaustion(&self) -> bool {
        match self {
            Self::Trap(e) => matches!(
                e.downcast_ref::<wasmtime::Trap>(),
                Some(wasmtime::Trap::OutOfFuel)
            ),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_decodes_the_guest_schema() {
        // {1: 2} and {1: 0, 2: [0, 3]} — the datboi-guest-transform
        // encoder's own test vectors, decoded here: one schema, two
        // codebases, byte-level agreement pinned.
        assert_eq!(
            Descriptor::from_cbor(&[0xa1, 0x01, 0x02]),
            Ok(Descriptor {
                seek: SeekClass::Opaque,
                random_access_inputs: vec![],
            })
        );
        assert_eq!(
            Descriptor::from_cbor(&[0xa2, 0x01, 0x00, 0x02, 0x82, 0x00, 0x03]),
            Ok(Descriptor {
                seek: SeekClass::Affine,
                random_access_inputs: vec![0, 3],
            })
        );
    }

    #[test]
    fn descriptor_ignores_advisory_keys_and_rejects_junk() {
        // {1: 1, 99: "future"} — unknown key ignored (D89 advisory rule).
        let with_future = [
            0xa2, 0x01, 0x01, 0x18, 0x63, 0x66, b'f', b'u', b't', b'u', b'r', b'e',
        ];
        assert_eq!(
            Descriptor::from_cbor(&with_future).map(|d| d.seek),
            Ok(SeekClass::ManifestSeekable)
        );
        // Missing seek, empty rai array, non-map: all refuse.
        assert!(Descriptor::from_cbor(&[0xa0]).is_err());
        assert!(Descriptor::from_cbor(&[0xa2, 0x01, 0x00, 0x02, 0x80]).is_err());
        assert!(Descriptor::from_cbor(&[0x01]).is_err());
    }
}
