//! Host for the frozen `datboi:transform@2` streaming world (D46/D49,
//! frozen 2026-07-07 after the M2 exit test — see the WIT header).
//!
//! Same determinism doctrine as the @1 host, plus the stream layer:
//!
//! * The linker gains EXACTLY our `types` resource methods — still no
//!   clock, random, or filesystem. The import surface stays the sandbox.
//! * The exact-read contract is enforced HERE: `source.read(n)` returns
//!   `n` bytes unless the stream ends (host loops over short reads from
//!   the underlying reader), so the guest-visible byte sequence can never
//!   depend on host buffering.
//! * Reads above [`MAX_READ`] trap deterministically — the resource-abuse
//!   guard that keeps a hostile guest from forcing multi-GB host
//!   allocations without breaking the exact-read contract with a clamp.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};

use crate::{Limits, RuntimeError, SeekClass};

// The generated `call_serve_range` mirrors the WIT's flat 7-arg shape.
#[allow(clippy::too_many_arguments)]
mod bindings {
    // Host bindings for the v2 world; host resources map to the
    // entry types below.
    wasmtime::component::bindgen!({
        world: "transform-stream",
        path: "../../wit/transform/v2",
        // Host methods return wasmtime::Result so contract violations
        // (MAX_READ) and sink I/O failures become deterministic traps.
        imports: { default: trappable },
        with: {
            "datboi:transform/types/source": super::SourceEntry,
            "datboi:transform/types/file": super::FileEntry,
            "datboi:transform/types/sink": super::SinkEntry,
        },
    });
}

use bindings::TransformStream;
use bindings::datboi::transform::types;

/// A single `read(n)` may not exceed this (16 MiB): larger requests trap,
/// deterministically, everywhere. Documented in the WIT.
pub const MAX_READ: u32 = 16 << 20;

/// Sequential input: any reader plus its total length (inputs are CAS
/// blobs; lengths are always known).
pub struct SequentialInput {
    pub reader: Box<dyn Read + Send>,
    pub len: u64,
}

/// Random-access input for `serve-range` (and `run` positions declared in
/// the descriptor).
#[allow(clippy::len_without_is_empty)] // zero-length inputs are ordinary
pub trait RangeRead: Send {
    /// Fill `buf` starting at `offset`; short only at end-of-file.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize>;
    fn len(&self) -> u64;
}

impl RangeRead for Vec<u8> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = usize::try_from(offset.min(self.len() as u64)).unwrap_or(usize::MAX);
        let end = (start + buf.len()).min(Vec::len(self));
        let n = end.saturating_sub(start);
        buf[..n].copy_from_slice(&self[start..end]);
        Ok(n)
    }
    fn len(&self) -> u64 {
        Vec::len(self) as u64
    }
}

/// A plain file as a random-access source (seek + read; `RangeRead`
/// takes `&mut self`, so the file's cursor is ours to move).
pub struct FileRandom {
    file: std::fs::File,
    len: u64,
}

impl FileRandom {
    /// # Errors
    /// If the file's length cannot be stat'd.
    pub fn new(file: std::fs::File) -> std::io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}

impl RangeRead for FileRandom {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        if offset >= self.len {
            return Ok(0);
        }
        self.file.seek(SeekFrom::Start(offset))?;
        let cap = usize::try_from((self.len - offset).min(buf.len() as u64)).expect("bounded");
        let mut filled = 0usize;
        while filled < cap {
            match self.file.read(&mut buf[filled..cap]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }

    fn len(&self) -> u64 {
        self.len
    }
}

/// One resolved input, recipe order.
pub enum StreamInput {
    Sequential(SequentialInput),
    RandomAccess(Box<dyn RangeRead>),
}

// ---- resource table entries (host side of the guest handles) ----

pub struct SourceEntry {
    reader: Box<dyn Read + Send>,
    len: u64,
    progress: Arc<SourceProgress>,
}

/// Host-side ledger for one sequential input, shared with the run call:
/// truncation evidence must survive the guest dropping its handle
/// (which deletes the [`SourceEntry`] from the table).
#[derive(Default)]
struct SourceProgress {
    consumed: AtomicU64,
    /// The reader hit EOF before `len` — the declared length is
    /// disproven, whatever the guest goes on to do with the short view.
    ended_early: AtomicBool,
}

pub struct FileEntry {
    inner: Box<dyn RangeRead>,
}

pub struct SinkEntry {
    writer: Box<dyn Write + Send>,
}

struct HostState {
    limits: StoreLimits,
    table: ResourceTable,
}

impl types::Host for HostState {}

impl types::HostSource for HostState {
    fn read(&mut self, this: Resource<SourceEntry>, n: u32) -> wasmtime::Result<Vec<u8>> {
        if n > MAX_READ {
            anyhow::bail!("read({n}) exceeds MAX_READ ({MAX_READ}): resource-abuse guard");
        }
        let entry = self.table.get_mut(&this)?;
        // EXACT contract: loop until n bytes or true end-of-stream; a
        // short read from the underlying reader is not guest-visible.
        let consumed = entry.progress.consumed.load(Ordering::Relaxed);
        let want =
            usize::try_from(u64::from(n).min(entry.len - consumed)).expect("bounded by MAX_READ");
        let mut buf = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            match entry.reader.read(&mut buf[filled..]) {
                // Reader ended before its declared len: record the
                // disproof for the post-run length check.
                Ok(0) => {
                    entry.progress.ended_early.store(true, Ordering::Relaxed);
                    break;
                }
                Ok(k) => filled += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        buf.truncate(filled);
        entry
            .progress
            .consumed
            .store(consumed + filled as u64, Ordering::Relaxed);
        Ok(buf)
    }

    fn len(&mut self, this: Resource<SourceEntry>) -> wasmtime::Result<u64> {
        Ok(self.table.get(&this)?.len)
    }

    fn drop(&mut self, this: Resource<SourceEntry>) -> wasmtime::Result<()> {
        self.table.delete(this)?;
        Ok(())
    }
}

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

/// A compiled @2 component, ready to instantiate cheaply.
pub struct StreamTransform {
    component: Component,
}

/// Which span of which output `serve_range` should produce.
#[derive(Debug, Clone, Copy)]
pub struct RangeRequest {
    pub output_ix: u32,
    pub offset: u64,
    pub len: u64,
}

/// Deterministically-configured host for `datboi:transform@2` components.
pub struct StreamHost {
    engine: Engine,
    linker: Linker<HostState>,
    limits: Limits,
}

impl StreamHost {
    /// Build a streaming host with the given resource limits.
    ///
    /// # Errors
    /// If wasmtime rejects the deterministic engine configuration.
    pub fn new(limits: Limits) -> Result<Self, RuntimeError> {
        let engine =
            Engine::new(&crate::deterministic_config()).map_err(RuntimeError::Component)?;
        // The ONLY imports: our own types interface. Still no ambient
        // capabilities — the stream methods are the whole import surface.
        let mut linker = Linker::new(&engine);
        TransformStream::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
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

    /// Compile a component once; run it many times. Compilation is the
    /// expensive step (cranelift), instantiation is ~microseconds — the
    /// executor replays thousands of recipes against a handful of pinned
    /// components, so the split is load-bearing, not a convenience.
    ///
    /// # Errors
    /// If the bytes are not a valid component.
    pub fn load(&self, component_bytes: &[u8]) -> Result<StreamTransform, RuntimeError> {
        // D54: anonymous components don't load — identity metadata is
        // required in-band so any pinned hash can be traced to a name,
        // description, source, and content revision.
        crate::attribution::parse_attribution(component_bytes)
            .map_err(|e| RuntimeError::Component(anyhow::anyhow!(e)))?;
        Ok(StreamTransform {
            component: Component::from_binary(&self.engine, component_bytes)
                .map_err(RuntimeError::Component)?,
        })
    }

    fn instantiate(
        &self,
        store: &mut Store<HostState>,
        transform: &StreamTransform,
    ) -> Result<TransformStream, RuntimeError> {
        TransformStream::instantiate(store, &transform.component, &self.linker)
            .map_err(RuntimeError::Instantiate)
    }

    /// Read a transform's static capability metadata for `op`.
    ///
    /// # Errors
    /// If the component is invalid, fails to instantiate, or traps.
    pub fn describe(
        &self,
        transform: &StreamTransform,
        op: &str,
    ) -> Result<crate::Descriptor, RuntimeError> {
        let mut store = self.store(None)?;
        let transform = self.instantiate(&mut store, transform)?;
        let d = transform
            .call_describe(&mut store, op)
            .map_err(RuntimeError::Trap)?;
        Ok(crate::Descriptor {
            seek: match d.seek {
                types::SeekClass::Affine => SeekClass::Affine,
                types::SeekClass::ManifestSeekable => SeekClass::ManifestSeekable,
                types::SeekClass::Opaque => SeekClass::Opaque,
            },
            random_access_inputs: d.random_access_inputs,
        })
    }

    /// Run one streaming operation to completion. Output bytes land in
    /// `sinks` (recipe order) as the guest produces them — the caller tees
    /// hashing/verification (D4) into its `Write` impls.
    ///
    /// # Errors
    /// [`RuntimeError::Component`] for an invalid binary,
    /// [`RuntimeError::Instantiate`] for link/world wiring failures,
    /// [`RuntimeError::Trap`] for traps / exhausted budgets,
    /// [`RuntimeError::Transform`] for guest-reported errors,
    /// [`RuntimeError::InputLengthMismatch`] when a sequential input
    /// ends short of its declared length (outranks the guest verdict —
    /// the guest computed on a truncated view).
    pub fn run(
        &self,
        transform: &StreamTransform,
        op: &str,
        params: &[u8],
        inputs: Vec<StreamInput>,
        sinks: Vec<Box<dyn Write + Send>>,
    ) -> Result<(), RuntimeError> {
        self.run_fueled(transform, op, params, inputs, sinks, None)
    }

    /// [`StreamHost::run`] with an explicit fuel budget — the executor
    /// scales fuel with a recipe's declared byte sizes so multi-GiB
    /// members don't trap on a flat default while runaway guests still
    /// die. `None` uses [`Limits::fuel`].
    ///
    /// # Errors
    /// As [`StreamHost::run`].
    pub fn run_fueled(
        &self,
        transform: &StreamTransform,
        op: &str,
        params: &[u8],
        inputs: Vec<StreamInput>,
        sinks: Vec<Box<dyn Write + Send>>,
        fuel: Option<u64>,
    ) -> Result<(), RuntimeError> {
        let mut store = self.store(fuel)?;
        let transform = self.instantiate(&mut store, transform)?;

        let mut input_handles = Vec::with_capacity(inputs.len());
        let mut ledgers: Vec<(u32, u64, Arc<SourceProgress>)> = Vec::new();
        for (ix, input) in inputs.into_iter().enumerate() {
            let handle = match input {
                StreamInput::Sequential(s) => {
                    let progress = Arc::new(SourceProgress::default());
                    ledgers.push((
                        u32::try_from(ix).expect("input count fits u32"),
                        s.len,
                        Arc::clone(&progress),
                    ));
                    types::Input::Sequential(
                        store
                            .data_mut()
                            .table
                            .push(SourceEntry {
                                reader: s.reader,
                                len: s.len,
                                progress,
                            })
                            .map_err(|e| RuntimeError::Component(e.into()))?,
                    )
                }
                StreamInput::RandomAccess(inner) => types::Input::RandomAccess(
                    store
                        .data_mut()
                        .table
                        .push(FileEntry { inner })
                        .map_err(|e| RuntimeError::Component(e.into()))?,
                ),
            };
            input_handles.push(handle);
        }
        let mut sink_handles = Vec::with_capacity(sinks.len());
        for writer in sinks {
            sink_handles.push(
                store
                    .data_mut()
                    .table
                    .push(SinkEntry { writer })
                    .map_err(|e| RuntimeError::Component(e.into()))?,
            );
        }

        let outcome = transform.call_run(&mut store, op, params, &input_handles, &sink_handles);
        // A sequential input that ended before its declared length
        // truncated the guest's view: whatever the guest then did —
        // trap, error, or wrong output bytes — the disproven claim is
        // the length's, so it outranks the guest verdict. The executor
        // uses the attribution to refuse without poisoning the parent.
        for (input_ix, claimed, progress) in &ledgers {
            if progress.ended_early.load(Ordering::Relaxed) {
                return Err(RuntimeError::InputLengthMismatch {
                    input_ix: *input_ix,
                    claimed: *claimed,
                    actual: progress.consumed.load(Ordering::Relaxed),
                });
            }
        }
        outcome
            .map_err(RuntimeError::Trap)?
            .map_err(RuntimeError::Transform)
    }

    /// Serve `output[output_ix][offset .. offset+len)` into `sink`. All
    /// inputs are random-access by contract (see the WIT).
    ///
    /// # Errors
    /// As [`StreamHost::run`].
    pub fn serve_range(
        &self,
        transform: &StreamTransform,
        op: &str,
        params: &[u8],
        inputs: Vec<Box<dyn RangeRead>>,
        range: RangeRequest,
        sink: Box<dyn Write + Send>,
    ) -> Result<(), RuntimeError> {
        self.serve_range_fueled(transform, op, params, inputs, range, sink, None)
    }

    /// [`StreamHost::serve_range`] with an explicit fuel budget; `None`
    /// uses [`Limits::fuel`].
    ///
    /// # Errors
    /// As [`StreamHost::run`].
    #[allow(clippy::too_many_arguments)]
    pub fn serve_range_fueled(
        &self,
        transform: &StreamTransform,
        op: &str,
        params: &[u8],
        inputs: Vec<Box<dyn RangeRead>>,
        range: RangeRequest,
        sink: Box<dyn Write + Send>,
        fuel: Option<u64>,
    ) -> Result<(), RuntimeError> {
        let mut store = self.store(fuel)?;
        let transform = self.instantiate(&mut store, transform)?;

        let mut input_handles = Vec::with_capacity(inputs.len());
        for inner in inputs {
            input_handles.push(types::Input::RandomAccess(
                store
                    .data_mut()
                    .table
                    .push(FileEntry { inner })
                    .map_err(|e| RuntimeError::Component(e.into()))?,
            ));
        }
        let sink_handle = store
            .data_mut()
            .table
            .push(SinkEntry { writer: sink })
            .map_err(|e| RuntimeError::Component(e.into()))?;

        transform
            .call_serve_range(
                &mut store,
                op,
                params,
                &input_handles,
                range.output_ix,
                range.offset,
                range.len,
                sink_handle,
            )
            .map_err(RuntimeError::Trap)?
            .map_err(RuntimeError::Transform)
    }
}
