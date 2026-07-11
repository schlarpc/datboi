//! Host for the `datboi:extractor@1` world (D58) — the extractor sibling
//! of the `datboi:transform@2` stream host.
//!
//! Same determinism doctrine (D5/D46): the linker gains EXACTLY this
//! world's `types` resources — a host-implemented seekable `file` (the
//! archive container) and `sink` (one member's bytes) — and nothing
//! ambient. The component (vendored unrar inside the sandbox) imports
//! nothing else; its own libc is shut out by its determinism shim.
//!
//! Two operations, mirroring the WIT:
//!   * `enumerate` — walk the container's headers, returning file-member
//!     metadata (name/size/crc/solid), refusing encrypted & multi-volume
//!     archives.
//!   * `extract` — decode one member (by its `enumerate` index) into a
//!     host sink, hash-teed and verified by the caller (D4).
//!
//! A guest trap (unrar's ErrHandler, allocation failure, any diagnostic
//! abort) surfaces as [`RuntimeError::Trap`] and refuses the whole archive,
//! matching the refuse-suspicious-archives posture. Fuel/epoch/memory
//! bounds are the same as the stream host — wasmtime's memory cap turns a
//! RAR5 big-dictionary bomb into a clean refusal.

use std::io::Write;

use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};

use crate::stream::{RangeRead, MAX_READ};
use crate::{Limits, RuntimeError};

mod bindings {
    wasmtime::component::bindgen!({
        world: "extractor",
        path: "../../transforms/wit/ex1",
        // Host methods return wasmtime::Result so contract violations
        // (MAX_READ) become deterministic traps.
        imports: { default: trappable },
        with: {
            "datboi:extractor/types/file": super::FileEntry,
            "datboi:extractor/types/sink": super::SinkEntry,
        },
    });
}

use bindings::Extractor;
use bindings::datboi::extractor::types;

pub use bindings::datboi::extractor::types::Member;

/// Host side of the guest's `file` handle: the archive container.
pub struct FileEntry {
    inner: Box<dyn RangeRead>,
}

/// Host side of the guest's `sink` handle: one member's output.
pub struct SinkEntry {
    writer: Box<dyn Write + Send>,
}

struct HostState {
    limits: StoreLimits,
    table: ResourceTable,
}

impl types::Host for HostState {}

impl types::HostFile for HostState {
    fn read_at(
        &mut self,
        this: Resource<FileEntry>,
        offset: u64,
        n: u32,
    ) -> wasmtime::Result<Vec<u8>> {
        if n > MAX_READ {
            anyhow::bail!("read-at({n}) exceeds MAX_READ ({MAX_READ}): resource-abuse guard");
        }
        let entry = self.table.get_mut(&this)?;
        // EXACT contract: fill n bytes unless the range passes EOF.
        let mut buf = vec![0u8; n as usize];
        let mut filled = 0;
        while filled < buf.len() {
            match entry
                .inner
                .read_at(offset + filled as u64, &mut buf[filled..])
            {
                Ok(0) => break,
                Ok(k) => filled += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }

    fn len(&mut self, this: Resource<FileEntry>) -> wasmtime::Result<u64> {
        Ok(self.table.get(&this)?.inner.len())
    }

    fn drop(&mut self, this: Resource<FileEntry>) -> wasmtime::Result<()> {
        self.table.delete(this)?;
        Ok(())
    }
}

impl types::HostSink for HostState {
    fn write(&mut self, this: Resource<SinkEntry>, chunk: Vec<u8>) -> wasmtime::Result<()> {
        let entry = self.table.get_mut(&this)?;
        entry.writer.write_all(&chunk)?;
        Ok(())
    }

    fn drop(&mut self, this: Resource<SinkEntry>) -> wasmtime::Result<()> {
        self.table.delete(this)?;
        Ok(())
    }
}

/// A compiled `datboi:extractor@1` component, ready to instantiate cheaply.
pub struct ExtractorComponent {
    component: Component,
}

/// Deterministically-configured host for extractor components.
pub struct ExtractorHost {
    engine: Engine,
    linker: Linker<HostState>,
    limits: Limits,
}

impl ExtractorHost {
    /// Build an extractor host with the given resource limits.
    ///
    /// # Errors
    /// If wasmtime rejects the deterministic engine configuration.
    pub fn new(limits: Limits) -> Result<Self, RuntimeError> {
        let engine =
            Engine::new(&crate::deterministic_config()).map_err(RuntimeError::Component)?;
        // The ONLY imports: this world's types interface (the archive file
        // and member sink). Still no ambient capabilities (D46).
        let mut linker = Linker::new(&engine);
        Extractor::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(RuntimeError::Component)?;
        Ok(Self {
            engine,
            linker,
            limits,
        })
    }

    fn store(&self, fuel: Option<u64>) -> Result<Store<HostState>, RuntimeError> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.memory)
            .build();
        let mut store = Store::new(
            &self.engine,
            HostState {
                limits,
                table: ResourceTable::new(),
            },
        );
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(fuel.unwrap_or(self.limits.fuel))
            .map_err(RuntimeError::Component)?;
        Ok(store)
    }

    /// Compile a component once; run it many times (D51 load/run split).
    ///
    /// # Errors
    /// If the bytes are not a valid component, or lack D54 attribution.
    pub fn load(&self, component_bytes: &[u8]) -> Result<ExtractorComponent, RuntimeError> {
        // D54: anonymous components don't load.
        crate::attribution::parse_attribution(component_bytes)
            .map_err(|e| RuntimeError::Component(anyhow::anyhow!(e)))?;
        Ok(ExtractorComponent {
            component: Component::from_binary(&self.engine, component_bytes)
                .map_err(RuntimeError::Component)?,
        })
    }

    fn instantiate(
        &self,
        store: &mut Store<HostState>,
        component: &ExtractorComponent,
    ) -> Result<Extractor, RuntimeError> {
        Extractor::instantiate(store, &component.component, &self.linker)
            .map_err(RuntimeError::Trap)
    }

    /// Enumerate the archive's file members.
    ///
    /// # Errors
    /// [`RuntimeError::Trap`] for a guest trap / exhausted budget,
    /// [`RuntimeError::Transform`] for a guest-reported refusal (encrypted,
    /// multi-volume, unparseable).
    pub fn enumerate(
        &self,
        component: &ExtractorComponent,
        archive: Box<dyn RangeRead>,
    ) -> Result<Vec<Member>, RuntimeError> {
        let mut store = self.store(None)?;
        let extractor = self.instantiate(&mut store, component)?;
        let handle = store
            .data_mut()
            .table
            .push(FileEntry { inner: archive })
            .map_err(|e| RuntimeError::Component(e.into()))?;
        extractor
            .call_enumerate(&mut store, handle)
            .map_err(RuntimeError::Trap)?
            .map_err(RuntimeError::Transform)
    }

    /// Decode member `ix` (as numbered by [`ExtractorHost::enumerate`]) into
    /// `sink`. The caller tees hashing/verification (D4) into its `Write`.
    ///
    /// # Errors
    /// As [`ExtractorHost::enumerate`].
    pub fn extract(
        &self,
        component: &ExtractorComponent,
        archive: Box<dyn RangeRead>,
        ix: u32,
        sink: Box<dyn Write + Send>,
    ) -> Result<(), RuntimeError> {
        self.extract_fueled(component, archive, ix, sink, None)
    }

    /// [`ExtractorHost::extract`] with an explicit fuel budget (solid
    /// archives decode predecessors, so the executor scales fuel with the
    /// container size). `None` uses [`Limits::fuel`].
    ///
    /// # Errors
    /// As [`ExtractorHost::enumerate`].
    pub fn extract_fueled(
        &self,
        component: &ExtractorComponent,
        archive: Box<dyn RangeRead>,
        ix: u32,
        sink: Box<dyn Write + Send>,
        fuel: Option<u64>,
    ) -> Result<(), RuntimeError> {
        let mut store = self.store(fuel)?;
        let extractor = self.instantiate(&mut store, component)?;
        let file = store
            .data_mut()
            .table
            .push(FileEntry { inner: archive })
            .map_err(|e| RuntimeError::Component(e.into()))?;
        let out = store
            .data_mut()
            .table
            .push(SinkEntry { writer: sink })
            .map_err(|e| RuntimeError::Component(e.into()))?;
        extractor
            .call_extract(&mut store, file, ix, out)
            .map_err(RuntimeError::Trap)?
            .map_err(RuntimeError::Transform)
    }
}
