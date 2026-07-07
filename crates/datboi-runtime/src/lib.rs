//! wasmtime host for content-addressed transforms (docs/30-runtime.md,
//! decisions D5–D7).
//!
//! This crate runs components targeting the frozen `datboi:transform@1`
//! world (transforms/wit/transform.wit). Its one job beyond "call the
//! component" is to make execution **deterministic and bounded**, because
//! storage recipes must replay bit-exact forever (D5) and peer-supplied
//! components are untrusted (their only threat is resource abuse — the CAS
//! verifies output bytes, D4).

use thiserror::Error;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

mod bindings {
    // Host bindings for the frozen world. Generated at compile time from the
    // shared WIT — the host and guest cannot drift because they read the same
    // file.
    wasmtime::component::bindgen!({
        world: "transform",
        path: "../../transforms/wit/v1",
    });
}

use bindings::Transform;

pub mod stream;

/// The deterministic engine configuration (D5) shared by the @1 and @2
/// hosts. Every knob here is part of the determinism contract.
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

/// Seekability class a transform declares (docs/80-views.md, D27). Mirrors
/// the WIT enum; kept as a hand-written host type so the rest of the daemon
/// depends on this crate's vocabulary, not on generated bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekClass {
    /// Output ranges map to input ranges arithmetically.
    Affine,
    /// Random access via a content-addressed index (frame/block tables).
    ManifestSeekable,
    /// Whole-stream only; range reads require materialization first.
    Opaque,
}

impl From<bindings::SeekClass> for SeekClass {
    fn from(s: bindings::SeekClass) -> Self {
        match s {
            bindings::SeekClass::Affine => Self::Affine,
            bindings::SeekClass::ManifestSeekable => Self::ManifestSeekable,
            bindings::SeekClass::Opaque => Self::Opaque,
        }
    }
}

/// A transform's static, pure capability metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub seek: SeekClass,
    pub random_access_inputs: Vec<u32>,
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
    #[error("transform trapped or exhausted its resource budget: {0}")]
    Trap(#[source] wasmtime::Error),
    #[error("transform returned an error: {0}")]
    Transform(String),
}

struct HostState {
    limits: StoreLimits,
}

/// A reusable, deterministically-configured wasmtime engine for running
/// transforms. Construct once (compiling the `Config` is the expensive part)
/// and run many components.
pub struct TransformHost {
    engine: Engine,
    linker: Linker<HostState>,
    limits: Limits,
}

impl TransformHost {
    /// Build a host with the given resource limits.
    ///
    /// # Errors
    /// If wasmtime rejects the deterministic engine configuration.
    pub fn new(limits: Limits) -> Result<Self, RuntimeError> {
        let engine = Engine::new(&deterministic_config()).map_err(RuntimeError::Component)?;
        // The world imports nothing (no clock/random/fs) — ambient
        // nondeterminism is unrepresentable, so the linker stays empty.
        let linker = Linker::new(&engine);
        Ok(Self {
            engine,
            linker,
            limits,
        })
    }

    fn store(&self) -> Result<Store<HostState>, RuntimeError> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.memory)
            .build();
        let mut store = Store::new(&self.engine, HostState { limits });
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(RuntimeError::Component)?;
        Ok(store)
    }

    fn instantiate(
        &self,
        store: &mut Store<HostState>,
        component_bytes: &[u8],
    ) -> Result<Transform, RuntimeError> {
        let component = Component::from_binary(&self.engine, component_bytes)
            .map_err(RuntimeError::Component)?;
        Transform::instantiate(store, &component, &self.linker).map_err(RuntimeError::Trap)
    }

    /// Read a transform's static capability metadata for `op`.
    ///
    /// # Errors
    /// If the component is invalid or traps.
    pub fn describe(&self, component_bytes: &[u8], op: &str) -> Result<Descriptor, RuntimeError> {
        let mut store = self.store()?;
        let transform = self.instantiate(&mut store, component_bytes)?;
        let d = transform
            .call_describe(&mut store, op)
            .map_err(RuntimeError::Trap)?;
        Ok(Descriptor {
            seek: d.seek.into(),
            random_access_inputs: d.random_access_inputs,
        })
    }

    /// Run one operation of a transform component: `op` selects the
    /// operation, `params` is the recipe's canonical-CBOR params, `inputs`
    /// are the resolved input blobs in recipe order. Returns the output blobs
    /// in recipe order.
    ///
    /// # Errors
    /// [`RuntimeError::Component`] for an invalid binary, [`RuntimeError::Trap`]
    /// for a trap or exhausted budget, [`RuntimeError::Transform`] for an
    /// error the transform itself returned.
    pub fn run(
        &self,
        component_bytes: &[u8],
        op: &str,
        params: &[u8],
        inputs: &[Vec<u8>],
    ) -> Result<Vec<Vec<u8>>, RuntimeError> {
        let mut store = self.store()?;
        let transform = self.instantiate(&mut store, component_bytes)?;
        transform
            .call_run(&mut store, op, params, inputs)
            .map_err(RuntimeError::Trap)?
            .map_err(RuntimeError::Transform)
    }
}
