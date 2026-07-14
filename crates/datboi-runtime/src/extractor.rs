//! Host for the `datboi:extractor@1` world (D58 pathfinder, D89 shape)
//! — the extractor sibling of the transform stream host.
//!
//! Same determinism doctrine (D5/D46): the linker gains EXACTLY the
//! `datboi:streams@1` resources — a host-implemented seekable `file`
//! (the archive containers) and `sink` (one member's bytes) — and
//! nothing ambient. The component imports nothing else.
//!
//! Two operations, mirroring the WIT:
//!   * `enumerate` — walk the containers' headers, returning the
//!     canonical-CBOR member list (decoded here with the schema mirror
//!     of `datboi-guest-extractor`'s encoder).
//!   * `extract` — decode a BATCH of members (by `enumerate` index)
//!     into host sinks, hash-teed and verified by the caller (D4). One
//!     call per ingest sweep is the point (D89): each solid block
//!     decodes once however many members are requested. Replay passes a
//!     one-request batch.
//!
//! A guest trap (a vendored parser's error handler, allocation failure,
//! any diagnostic abort) surfaces as [`RuntimeError::Trap`] and refuses
//! the whole archive, matching the refuse-suspicious-archives posture.
//! Fuel/epoch/memory bounds are the same as the stream host — wasmtime's
//! memory cap turns a decompression bomb into a clean refusal.

use std::io::Write;

use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};

use crate::stream::{MAX_READ, RangeRead};
use crate::{Limits, RuntimeError};

mod bindings {
    wasmtime::component::bindgen!({
        world: "extractor",
        path: "../../wit/extractor/v1",
        // Host methods return wasmtime::Result so contract violations
        // (MAX_READ) become deterministic traps.
        imports: { default: trappable },
        with: {
            // The world only uses `file` and `sink`, but importing the
            // streams interface imports all of it — map `source` to the
            // stream host's entry so the trait impl below can refuse it
            // with one shared type.
            "datboi:streams/types/source": crate::stream::SourceEntry,
            "datboi:streams/types/file": super::FileEntry,
            "datboi:streams/types/sink": super::SinkEntry,
        },
    });
}

use bindings::Extractor;
use bindings::datboi::extractor::types::ExtractRequest;
use bindings::datboi::streams::types as streams;

/// One archive member, decoded from `enumerate`'s canonical-CBOR member
/// list (schema `{1: [member...]}`, member `{1: ix, 2: name, 3: size,
/// 4: packed-size, 5: crc32, 6: solid}` — encoder and doc in
/// `datboi-guest-extractor`). `ix` is the member's stable identity
/// within the ordered container list; `name` is metadata, never an
/// identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub ix: u32,
    pub name: String,
    pub size: u64,
    pub packed_size: u64,
    pub crc32: u32,
    pub solid: bool,
}

/// Decode the member list. Unknown keys (top-level and per-member) are
/// IGNORED — the D89 advisory rule; known keys are strictly checked
/// (the house canonical decoder already rejects non-canonical bytes).
///
/// # Errors
/// On non-canonical CBOR or missing/ill-typed required keys.
pub fn decode_members(bytes: &[u8]) -> Result<Vec<Member>, String> {
    use datboi_core::cbor::{self, Value};
    let Ok(Value::Map(entries)) = cbor::decode(bytes) else {
        return Err("member list is not a canonical CBOR map".into());
    };
    let Some((_, Value::Array(items))) = entries.into_iter().find(|(k, _)| *k == 1) else {
        return Err("member list missing the members array (key 1)".into());
    };
    let mut members = Vec::with_capacity(items.len());
    for item in items {
        let Value::Map(fields) = item else {
            return Err("member entry is not a map".into());
        };
        let mut ix = None;
        let mut name = None;
        let mut size = None;
        let mut packed_size = None;
        let mut crc32 = None;
        let mut solid = None;
        for (key, value) in fields {
            match (key, value) {
                (1, Value::Uint(v)) => {
                    ix = Some(u32::try_from(v).map_err(|_| "member ix out of range")?);
                }
                (2, Value::Text(v)) => name = Some(v),
                (3, Value::Uint(v)) => size = Some(v),
                (4, Value::Uint(v)) => packed_size = Some(v),
                (5, Value::Uint(v)) => {
                    crc32 = Some(u32::try_from(v).map_err(|_| "member crc32 out of range")?);
                }
                (6, Value::Uint(v)) => {
                    solid = Some(match v {
                        0 => false,
                        1 => true,
                        _ => return Err("member solid flag is not 0/1".into()),
                    });
                }
                // A known key with the wrong type is a schema violation,
                // not an advisory addition.
                (k @ 1..=6, _) => return Err(format!("member key {k} has the wrong type")),
                // Advisory keys from a newer component: ignored (D89).
                _ => {}
            }
        }
        members.push(Member {
            ix: ix.ok_or("member missing ix")?,
            name: name.ok_or("member missing name")?,
            size: size.ok_or("member missing size")?,
            packed_size: packed_size.ok_or("member missing packed-size")?,
            crc32: crc32.ok_or("member missing crc32")?,
            solid: solid.ok_or("member missing solid flag")?,
        });
    }
    Ok(members)
}

/// Host side of the guest's `file` handle: one archive container.
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

impl streams::Host for HostState {}
impl bindings::datboi::extractor::types::Host for HostState {}

impl streams::HostFile for HostState {
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

impl streams::HostSource for HostState {
    fn read(
        &mut self,
        _this: Resource<crate::stream::SourceEntry>,
        _n: u32,
    ) -> wasmtime::Result<Vec<u8>> {
        // The extractor world never receives a `source` (containers are
        // seek-driven by contract); a call here is a linker-wiring bug.
        anyhow::bail!("extractor world has no sequential sources")
    }

    fn len(&mut self, _this: Resource<crate::stream::SourceEntry>) -> wasmtime::Result<u64> {
        anyhow::bail!("extractor world has no sequential sources")
    }

    fn drop(&mut self, this: Resource<crate::stream::SourceEntry>) -> wasmtime::Result<()> {
        self.table.delete(this)?;
        Ok(())
    }
}

impl streams::HostSink for HostState {
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
        // The ONLY imports: the shared streams resources (D46/D89).
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
            .map_err(RuntimeError::Instantiate)
    }

    /// Enumerate the containers' file members.
    ///
    /// # Errors
    /// [`RuntimeError::Trap`] for a guest trap / exhausted budget,
    /// [`RuntimeError::Transform`] for a guest-reported refusal
    /// (encrypted, unsupported container set, refused params,
    /// unparseable) or a malformed member list.
    pub fn enumerate(
        &self,
        component: &ExtractorComponent,
        archives: Vec<Box<dyn RangeRead>>,
        params: &[u8],
    ) -> Result<Vec<Member>, RuntimeError> {
        let mut store = self.store(None)?;
        let extractor = self.instantiate(&mut store, component)?;
        let mut handles = Vec::with_capacity(archives.len());
        for archive in archives {
            handles.push(
                store
                    .data_mut()
                    .table
                    .push(FileEntry { inner: archive })
                    .map_err(|e| RuntimeError::Component(e.into()))?,
            );
        }
        let bytes = extractor
            .call_enumerate(&mut store, &handles, params)
            .map_err(RuntimeError::Trap)?
            .map_err(RuntimeError::Transform)?;
        decode_members(&bytes)
            .map_err(|e| RuntimeError::Transform(format!("malformed member list: {e}")))
    }

    /// Decode the requested members (as numbered by
    /// [`ExtractorHost::enumerate`]) into their sinks, one pass. The
    /// caller tees hashing/verification (D4) into its `Write`s.
    ///
    /// # Errors
    /// As [`ExtractorHost::enumerate`].
    pub fn extract(
        &self,
        component: &ExtractorComponent,
        archives: Vec<Box<dyn RangeRead>>,
        params: &[u8],
        requests: Vec<(u32, Box<dyn Write + Send>)>,
    ) -> Result<(), RuntimeError> {
        self.extract_fueled(component, archives, params, requests, None)
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
        archives: Vec<Box<dyn RangeRead>>,
        params: &[u8],
        requests: Vec<(u32, Box<dyn Write + Send>)>,
        fuel: Option<u64>,
    ) -> Result<(), RuntimeError> {
        let mut store = self.store(fuel)?;
        let extractor = self.instantiate(&mut store, component)?;
        let mut archive_handles = Vec::with_capacity(archives.len());
        for archive in archives {
            archive_handles.push(
                store
                    .data_mut()
                    .table
                    .push(FileEntry { inner: archive })
                    .map_err(|e| RuntimeError::Component(e.into()))?,
            );
        }
        let mut request_handles = Vec::with_capacity(requests.len());
        for (ix, writer) in requests {
            request_handles.push(ExtractRequest {
                ix,
                out: store
                    .data_mut()
                    .table
                    .push(SinkEntry { writer })
                    .map_err(|e| RuntimeError::Component(e.into()))?,
            });
        }
        extractor
            .call_extract(&mut store, &archive_handles, params, &request_handles)
            .map_err(RuntimeError::Trap)?
            .map_err(RuntimeError::Transform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_list_decodes_the_guest_schema() {
        // The datboi-guest-extractor encoder's own test vector.
        let bytes = [
            0xa1, 0x01, 0x81, 0xa6, 0x01, 0x00, 0x02, 0x61, b'a', 0x03, 0x03, 0x04, 0x02, 0x05,
            0x00, 0x06, 0x00,
        ];
        assert_eq!(
            decode_members(&bytes),
            Ok(vec![Member {
                ix: 0,
                name: "a".into(),
                size: 3,
                packed_size: 2,
                crc32: 0,
                solid: false,
            }])
        );
        assert_eq!(decode_members(&[0xa1, 0x01, 0x80]), Ok(vec![]));
        // Missing required key, bad solid flag, non-map: all refuse.
        assert!(decode_members(&[0xa1, 0x01, 0x81, 0xa0]).is_err());
        assert!(decode_members(&[0x80]).is_err());
    }
}
