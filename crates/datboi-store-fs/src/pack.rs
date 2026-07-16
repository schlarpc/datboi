//! D91 sealed packs: one immutable pack file per decomposition, pieces
//! in coverage order, a self-describing footer — the D19 packing
//! clause's first exercise. Identities never change: a packed blob's
//! hash is the member's own bytes' blake3, the pack file is just where
//! those bytes live. Packs are write-once (tmp → fsync → rename, like
//! every store artifact) and never mutated; "removing" a member is a
//! future tombstone-and-repack under the gc guard, not an edit.
//!
//! Resolution is store-internal and transparent: [`Store::open`] scans
//! the pack shard tree, parses each footer (one small tail read per
//! pack — packs are O(decompositions), never O(pieces)), and serves
//! `get`/`has`/`len` for packed members out of an in-memory map. The
//! map is derivable state; the footers are the truth (D15: recovery
//! re-parses them, no database required). Consumers can't tell a
//! packed blob from a loose one — which is exactly the point: every
//! read path in the system inherits pack support by construction.
//!
//! Format (little-endian throughout):
//!
//! ```text
//! [member 0 bytes][member 1 bytes]…
//! [footer: b"datboi/pack/1\n"
//!          u32 member count
//!          per member: 32-byte blake3, u64 offset, u64 len]
//! [u64 footer_len][b"DBOIPACK"]
//! ```
//!
//! The trailer magic + length locate the footer from the end; the
//! footer magic versions the format (a v2 is a new magic, D51-style —
//! shipped layouts are frozen). Member obao sidecars are NOT written
//! at pack time: packed pieces serve ranges through the D4 plain-read
//! default for literals, and a future `ensure_obao` over the window
//! upgrades them without touching the pack.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use datboi_core::alias::{AliasHasher, AliasTuple};
use datboi_core::hash::Blake3;

use crate::store::{Store, StoreError, fsync_dir};

/// `scan_packs` result: the resolution map + paths that refused to
/// parse (skipped, reported to the caller's logging).
pub(crate) type PackScan = (HashMap<Blake3, PackedLoc>, Vec<PathBuf>);

/// A written pack's identity + its member table rows.
type SealedPack = (Blake3, Vec<(Blake3, u64, u64)>);

/// The outcome of scrubbing one sealed pack ([`Store::scrub_pack`]).
#[derive(Debug)]
pub struct PackScrub {
    pub pack: Blake3,
    /// The pack file re-hashed to its own identity. `false` means the
    /// file rotted somewhere (member bytes, footer, or trailer); `true`
    /// proves every member by construction.
    pub intact: bool,
    /// One row per member, in offset order.
    pub members: Vec<PackMemberScrub>,
}

/// The outcome of a tombstone-and-repack ([`Store::repack`]).
#[derive(Debug)]
pub struct RepackOutcome {
    /// The surviving pack's new identity, or `None` if every member was
    /// dropped and the pack file is gone.
    pub new_pack: Option<Blake3>,
    /// Members actually dropped (present in the pack AND in the drop set).
    pub dropped: Vec<Blake3>,
    /// Bytes freed — the summed lengths of the dropped members.
    pub bytes_freed: u64,
}

/// One member's scrub row: its identity, length, and the alias tuple
/// re-derived from its bytes — `None` only if the member's own slice
/// failed to hash to its identity.
#[derive(Debug)]
pub struct PackMemberScrub {
    pub hash: Blake3,
    pub len: u64,
    pub aliases: Option<AliasTuple>,
}

pub(crate) const PACK_MAGIC: &[u8] = b"datboi/pack/1\n";
pub(crate) const PACK_TRAILER: &[u8] = b"DBOIPACK";
/// Trailer: u64 footer length + 8-byte magic.
const TRAILER_LEN: u64 = 16;
const MEMBER_ROW: usize = 32 + 8 + 8;

/// One member a caller intends to pack: identity + exact length,
/// both verified during the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackMember {
    pub hash: Blake3,
    pub len: u64,
}

/// Where a packed blob's bytes live (store-internal).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PackedLoc {
    pub pack: Blake3,
    pub offset: u64,
    pub len: u64,
}

/// An open blob: a loose file, or a bounded window of an immutable
/// pack. Reads and seeks are window-relative; a consumer cannot
/// escape the member's bytes.
pub struct Blob {
    inner: BlobInner,
}

enum BlobInner {
    Loose(File),
    Packed {
        file: File,
        start: u64,
        len: u64,
        pos: u64,
    },
}

impl Blob {
    /// Wrap a plain file (the loose-blob and spill-temp shape).
    #[must_use]
    pub fn loose(file: File) -> Self {
        Self {
            inner: BlobInner::Loose(file),
        }
    }

    pub(crate) fn packed(mut file: File, start: u64, len: u64) -> io::Result<Self> {
        file.seek(SeekFrom::Start(start))?;
        Ok(Self {
            inner: BlobInner::Packed {
                file,
                start,
                len,
                pos: 0,
            },
        })
    }

    /// Byte length of the blob (not the underlying pack).
    pub fn byte_len(&self) -> io::Result<u64> {
        match &self.inner {
            BlobInner::Loose(f) => Ok(f.metadata()?.len()),
            BlobInner::Packed { len, .. } => Ok(*len),
        }
    }
}

impl From<File> for Blob {
    fn from(file: File) -> Self {
        Self::loose(file)
    }
}

impl Read for Blob {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            BlobInner::Loose(f) => f.read(buf),
            BlobInner::Packed { file, len, pos, .. } => {
                let remaining = len.saturating_sub(*pos);
                if remaining == 0 {
                    return Ok(0);
                }
                let cap = usize::try_from(remaining.min(buf.len() as u64)).expect("bounded");
                let n = file.read(&mut buf[..cap])?;
                *pos += n as u64;
                Ok(n)
            }
        }
    }
}

impl positioned_io::ReadAt for Blob {
    // (File's own ReadAt provides the pread underneath.)
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        match &self.inner {
            BlobInner::Loose(f) => f.read_at(pos, buf),
            BlobInner::Packed {
                file, start, len, ..
            } => {
                let remaining = len.saturating_sub(pos.min(*len));
                if remaining == 0 {
                    return Ok(0);
                }
                let cap = usize::try_from(remaining.min(buf.len() as u64)).expect("bounded");
                file.read_at(start + pos, &mut buf[..cap])
            }
        }
    }
}

impl Seek for Blob {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        match &mut self.inner {
            BlobInner::Loose(f) => f.seek(target),
            BlobInner::Packed {
                file,
                start,
                len,
                pos,
            } => {
                let base: i128 = match target {
                    SeekFrom::Start(o) => i128::from(o),
                    SeekFrom::End(d) => i128::from(*len) + i128::from(d),
                    SeekFrom::Current(d) => i128::from(*pos) + i128::from(d),
                };
                let new = u64::try_from(base).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "seek before window start")
                })?;
                // Seeking past the end is legal (reads then return 0);
                // the underlying cursor clamps so reads stay in-window.
                file.seek(SeekFrom::Start(start.saturating_add(new.min(*len))))?;
                *pos = new;
                Ok(new)
            }
        }
    }
}

/// Parse a pack file's footer into its member table.
///
/// # Errors
/// A malformed footer (wrong magic, truncated table, rows outside the
/// file) — the pack is refused whole; members it held stay resolvable
/// only if another copy exists.
pub(crate) fn parse_footer(file: &mut File) -> Result<Vec<(Blake3, u64, u64)>, String> {
    let total = file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    if total < TRAILER_LEN {
        return Err("shorter than the trailer".into());
    }
    file.seek(SeekFrom::End(
        -i64::try_from(TRAILER_LEN).expect("trailer fits i64"),
    ))
    .map_err(|e| e.to_string())?;
    let mut trailer = [0u8; 16];
    file.read_exact(&mut trailer).map_err(|e| e.to_string())?;
    if &trailer[8..] != PACK_TRAILER {
        return Err("missing trailer magic".into());
    }
    let footer_len = u64::from_le_bytes(trailer[..8].try_into().expect("eight bytes"));
    let data_end = total
        .checked_sub(TRAILER_LEN)
        .and_then(|v| v.checked_sub(footer_len))
        .ok_or("footer length exceeds the file")?;
    file.seek(SeekFrom::Start(data_end)).map_err(|e| e.to_string())?;
    let mut footer = vec![0u8; usize::try_from(footer_len).map_err(|e| e.to_string())?];
    file.read_exact(&mut footer).map_err(|e| e.to_string())?;
    let table = footer
        .strip_prefix(PACK_MAGIC)
        .ok_or("missing footer magic")?;
    if table.len() < 4 {
        return Err("truncated member count".into());
    }
    let count = u32::from_le_bytes(table[..4].try_into().expect("four bytes")) as usize;
    let rows = &table[4..];
    if rows.len() != count * MEMBER_ROW {
        return Err("member table length disagrees with the count".into());
    }
    let mut members = Vec::with_capacity(count);
    for row in rows.chunks_exact(MEMBER_ROW) {
        let hash = Blake3(row[..32].try_into().expect("thirty-two bytes"));
        let offset = u64::from_le_bytes(row[32..40].try_into().expect("eight bytes"));
        let len = u64::from_le_bytes(row[40..48].try_into().expect("eight bytes"));
        if offset.checked_add(len).is_none_or(|end| end > data_end) {
            return Err("member row points outside the pack data".into());
        }
        members.push((hash, offset, len));
    }
    Ok(members)
}

pub(crate) fn encode_footer(members: &[(Blake3, u64, u64)]) -> Vec<u8> {
    let mut footer = Vec::with_capacity(PACK_MAGIC.len() + 4 + members.len() * MEMBER_ROW);
    footer.extend_from_slice(PACK_MAGIC);
    footer.extend_from_slice(
        &u32::try_from(members.len())
            .expect("member count fits u32")
            .to_le_bytes(),
    );
    for (hash, offset, len) in members {
        footer.extend_from_slice(&hash.0);
        footer.extend_from_slice(&offset.to_le_bytes());
        footer.extend_from_slice(&len.to_le_bytes());
    }
    footer
}

impl Store {
    /// Write one sealed pack: members streamed in order, each verified
    /// against its declared hash and length as it lands (a mismatch
    /// aborts the whole pack — nothing is published). The pack's own
    /// identity is the blake3 of the finished file; the write is the
    /// house tmp → fsync → rename discipline. Members become
    /// immediately resolvable through `get`/`has`/`len`.
    ///
    /// # Errors
    /// Member hash/length mismatches, I/O.
    pub fn put_pack<'r>(
        &self,
        members: &[PackMember],
        mut open: impl FnMut(usize) -> io::Result<Box<dyn Read + 'r>>,
    ) -> Result<Blake3, StoreError> {
        if members.is_empty() {
            return Err(StoreError::EmptyPack);
        }
        let temp = self.staging_path("pack");
        let result = self.write_pack_at(&temp, members, &mut open);
        match result {
            Ok((pack_hash, table)) => {
                let final_path = self.pack_path(&pack_hash);
                let parent = final_path.parent().expect("pack paths have parents");
                fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
                fs::rename(&temp, &final_path).map_err(|e| StoreError::io(&final_path, e))?;
                fsync_dir(parent)?;
                let mut packs = self.packs.write().unwrap_or_else(|e| e.into_inner());
                for (hash, offset, len) in table {
                    packs.insert(
                        hash,
                        PackedLoc {
                            pack: pack_hash,
                            offset,
                            len,
                        },
                    );
                }
                Ok(pack_hash)
            }
            Err(e) => {
                let _ = fs::remove_file(&temp);
                Err(e)
            }
        }
    }

    fn write_pack_at<'r>(
        &self,
        temp: &PathBuf,
        members: &[PackMember],
        open: &mut impl FnMut(usize) -> io::Result<Box<dyn Read + 'r>>,
    ) -> Result<SealedPack, StoreError> {
        let file = File::create(temp).map_err(|e| StoreError::io(temp, e))?;
        let mut out = HashingWriter {
            inner: io::BufWriter::new(file),
            hasher: blake3::Hasher::new(),
            written: 0,
        };
        let mut table = Vec::with_capacity(members.len());
        for (ix, member) in members.iter().enumerate() {
            let offset = out.written;
            let mut reader = open(ix).map_err(|e| StoreError::io(temp, e))?;
            let mut member_hasher = blake3::Hasher::new();
            let mut buf = vec![0u8; 64 * 1024];
            let mut copied = 0u64;
            loop {
                let n = reader.read(&mut buf).map_err(|e| StoreError::io(temp, e))?;
                if n == 0 {
                    break;
                }
                member_hasher.update(&buf[..n]);
                out.write_all(&buf[..n]).map_err(|e| StoreError::io(temp, e))?;
                copied += n as u64;
            }
            let got = Blake3(*member_hasher.finalize().as_bytes());
            if got != member.hash || copied != member.len {
                return Err(StoreError::PackMemberMismatch {
                    expected: member.hash,
                    got,
                    expected_len: member.len,
                    got_len: copied,
                });
            }
            table.push((member.hash, offset, member.len));
        }
        let footer = encode_footer(&table);
        out.write_all(&footer).map_err(|e| StoreError::io(temp, e))?;
        out.write_all(&(footer.len() as u64).to_le_bytes())
            .map_err(|e| StoreError::io(temp, e))?;
        out.write_all(PACK_TRAILER)
            .map_err(|e| StoreError::io(temp, e))?;
        let pack_hash = Blake3(*out.hasher.finalize().as_bytes());
        let mut file = out
            .inner
            .into_inner()
            .map_err(|e| StoreError::io(temp, e.into_error()))?;
        file.flush().map_err(|e| StoreError::io(temp, e))?;
        file.sync_all().map_err(|e| StoreError::io(temp, e))?;
        Ok((pack_hash, table))
    }

    /// Resolve a packed blob's location, if any (pack-map lookup).
    pub(crate) fn packed_loc(&self, hash: &Blake3) -> Option<PackedLoc> {
        self.packs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .copied()
    }

    /// Is this blob served out of a pack? (Loose copies win reads, but
    /// eviction planners must refuse blobs whose only local bytes are
    /// pack members — packs are immutable.)
    #[must_use]
    pub fn is_packed(&self, hash: &Blake3) -> bool {
        self.packed_loc(hash).is_some()
    }

    /// Every packed member `(hash, len)` — the recovery-scan
    /// complement of [`Store::list`], which walks loose files only. A
    /// rebuilt index must see packed pieces or every evicted
    /// container's grounding silently breaks after bare-NAS recovery.
    #[must_use]
    pub fn list_packed(&self) -> Vec<(Blake3, u64)> {
        let packs = self.packs.read().unwrap_or_else(|e| e.into_inner());
        let mut members: Vec<(Blake3, u64)> =
            packs.iter().map(|(hash, loc)| (*hash, loc.len)).collect();
        members.sort_unstable_by_key(|(hash, _)| hash.0);
        members
    }

    /// Every pack's identity, for scrub and recovery surfaces.
    #[must_use]
    pub fn list_packs(&self) -> Vec<Blake3> {
        let packs = self.packs.read().unwrap_or_else(|e| e.into_inner());
        let mut ids: Vec<Blake3> = packs.values().map(|loc| loc.pack).collect();
        ids.sort_unstable_by_key(|id| id.0);
        ids.dedup();
        ids
    }

    /// Scrub one sealed pack (D91): re-hash the whole file against its
    /// own identity (the filename) and re-derive every member's alias
    /// tuple, all in ONE sequential read. The whole-file check is the
    /// strongest and cheapest integrity proof a pack can get — `put_pack`
    /// verified each member's bytes INTO the hashed file, so a matching
    /// whole-file hash proves every member by construction (member bytes,
    /// footer, and trailer alike). The per-member tuples ride the same
    /// read for the fast-recovery alias back-fill (`scrub`'s second job).
    ///
    /// Packs are O(decompositions), so scrub re-reads each fully rather
    /// than sampling — they are the newest artifacts and the coverage
    /// gap the loose walk (`Store::list`) never touches.
    ///
    /// # Errors
    /// Missing/unreadable pack file, or a footer that no longer parses
    /// (structural rot — the open-time scan would refuse it too).
    pub fn scrub_pack(&self, pack: &Blake3) -> Result<PackScrub, StoreError> {
        let path = self.pack_path(pack);
        let mut file = File::open(&path).map_err(|e| StoreError::io(&path, e))?;
        let mut members = parse_footer(&mut file).map_err(|detail| StoreError::PackFooter {
            path: path.clone(),
            detail,
        })?;
        // Coverage order = write order = read order; drive per-member
        // hashers as the single forward pass crosses their spans.
        members.sort_unstable_by_key(|(_, offset, _)| *offset);
        file.seek(SeekFrom::Start(0)).map_err(|e| StoreError::io(&path, e))?;
        let mut whole = blake3::Hasher::new();
        let mut hashers: Vec<AliasHasher> = members.iter().map(|_| AliasHasher::new()).collect();
        let mut reader = io::BufReader::new(&mut file);
        let mut buf = vec![0u8; 64 * 1024];
        let mut pos = 0u64;
        let mut cur = 0usize;
        loop {
            let n = reader.read(&mut buf).map_err(|e| StoreError::io(&path, e))?;
            if n == 0 {
                break;
            }
            whole.update(&buf[..n]);
            let (chunk_start, chunk_end) = (pos, pos + n as u64);
            // Fan the buffer out over the members it overlaps. Footer and
            // trailer bytes past the last member fall through to `whole`
            // only. At most one member straddles any buffer boundary.
            while cur < members.len() {
                let (_, offset, len) = members[cur];
                let (mstart, mend) = (offset, offset + len);
                if mstart >= chunk_end {
                    break;
                }
                let lo = mstart.max(chunk_start);
                let hi = mend.min(chunk_end);
                if lo < hi {
                    let (a, b) = ((lo - chunk_start) as usize, (hi - chunk_start) as usize);
                    hashers[cur].update(&buf[a..b]);
                }
                if mend <= chunk_end {
                    cur += 1;
                } else {
                    break;
                }
            }
            pos = chunk_end;
        }
        let intact = Blake3(*whole.finalize().as_bytes()) == *pack;
        let members = members
            .into_iter()
            .zip(hashers)
            .map(|((hash, _, len), hasher)| {
                let aliases = hasher.finalize();
                PackMemberScrub {
                    hash,
                    len,
                    // Trusted for back-fill on its own merits: the slice's
                    // own bytes hashed to its identity.
                    aliases: (aliases.blake3 == hash).then_some(aliases),
                }
            })
            .collect();
        Ok(PackScrub {
            pack: *pack,
            intact,
            members,
        })
    }

    /// Which pack holds this blob's bytes, if any — the eviction/GC
    /// complement of [`Store::is_packed`] (the orphan path needs the
    /// pack identity to repack it).
    #[must_use]
    pub fn pack_of(&self, hash: &Blake3) -> Option<Blake3> {
        self.packed_loc(hash).map(|loc| loc.pack)
    }

    /// Tombstone-and-repack (D91 escape hatch): rewrite `pack` WITHOUT
    /// the members in `drop`, freeing their bytes. Packs are immutable,
    /// so "removing" a member means writing a fresh sealed pack of the
    /// survivors (streamed and re-verified straight out of the old
    /// windows), flipping the map, and unlinking the old file — or, if
    /// every member is dropped, unlinking outright. The CALLER owns the
    /// D73 safety (dropped members are orphaned + aged + unkept, under
    /// the gc guard); this only moves bytes. Returns what happened.
    ///
    /// # Errors
    /// Missing/unreadable pack, an unparseable footer, or I/O during the
    /// rewrite (the old pack is left intact on any failure — the new
    /// pack publishes atomically or not at all).
    pub fn repack(
        &self,
        pack: &Blake3,
        drop: &std::collections::HashSet<Blake3>,
    ) -> Result<RepackOutcome, StoreError> {
        let old_path = self.pack_path(pack);
        let members = {
            let mut f = File::open(&old_path).map_err(|e| StoreError::io(&old_path, e))?;
            parse_footer(&mut f).map_err(|detail| StoreError::PackFooter {
                path: old_path.clone(),
                detail,
            })?
        };
        let mut survivors: Vec<(Blake3, u64, u64)> = Vec::new();
        let mut dropped: Vec<Blake3> = Vec::new();
        let mut bytes_freed = 0u64;
        for (hash, offset, len) in &members {
            if drop.contains(hash) {
                dropped.push(*hash);
                bytes_freed += *len;
            } else {
                survivors.push((*hash, *offset, *len));
            }
        }
        // Nothing to drop (the caller's set named no member of this
        // pack): a no-op, the pack stands.
        if dropped.is_empty() {
            return Ok(RepackOutcome {
                new_pack: Some(*pack),
                dropped,
                bytes_freed: 0,
            });
        }
        // Whole pack is garbage: unlink and forget, no rewrite.
        if survivors.is_empty() {
            {
                let mut map = self.packs.write().unwrap_or_else(|e| e.into_inner());
                for (hash, _, _) in &members {
                    map.remove(hash);
                }
            }
            match fs::remove_file(&old_path) {
                Ok(()) => fsync_dir(old_path.parent().expect("pack paths have parents"))?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(StoreError::io(&old_path, e)),
            }
            return Ok(RepackOutcome {
                new_pack: None,
                dropped,
                bytes_freed,
            });
        }
        // Rewrite the survivors (coverage order) into a fresh pack,
        // streaming each straight out of the old pack's window — which
        // re-verifies every survivor against its hash as it lands.
        survivors.sort_unstable_by_key(|(_, offset, _)| *offset);
        let pack_members: Vec<PackMember> = survivors
            .iter()
            .map(|(hash, _, len)| PackMember {
                hash: *hash,
                len: *len,
            })
            .collect();
        let old = File::open(&old_path).map_err(|e| StoreError::io(&old_path, e))?;
        let new_hash = self.put_pack(&pack_members, |ix| {
            let (_, offset, len) = survivors[ix];
            Ok(Box::new(Blob::packed(old.try_clone()?, offset, len)?))
        })?;
        // put_pack remapped the survivors onto the new pack. Forget the
        // dropped members and unlink the superseded file.
        {
            let mut map = self.packs.write().unwrap_or_else(|e| e.into_inner());
            for hash in &dropped {
                map.remove(hash);
            }
        }
        if new_hash != *pack {
            match fs::remove_file(&old_path) {
                Ok(()) => fsync_dir(old_path.parent().expect("pack paths have parents"))?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(StoreError::io(&old_path, e)),
            }
        }
        Ok(RepackOutcome {
            new_pack: Some(new_hash),
            dropped,
            bytes_freed,
        })
    }

    pub(crate) fn pack_path(&self, hash: &Blake3) -> PathBuf {
        let hex = hash.to_hex();
        self.root()
            .join("packs")
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(hex)
    }

    /// Scan the pack shard tree and (re)build the resolution map — one
    /// footer read per pack. Called by `Store::open`; recovery calls it
    /// implicitly by reopening. Malformed packs are skipped with their
    /// paths reported (bytes-are-truth: a bad footer never aborts the
    /// store, it just fails to resolve).
    pub(crate) fn scan_packs(root: &std::path::Path) -> PackScan {
        let mut map = HashMap::new();
        let mut bad = Vec::new();
        let packs_root = root.join("packs");
        let mut stack = vec![packs_root];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for entry in rd.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    bad.push(path);
                    continue;
                };
                let Ok(pack_hash) = name.parse::<Blake3>() else {
                    bad.push(path);
                    continue;
                };
                let Ok(mut file) = File::open(&path) else {
                    bad.push(path);
                    continue;
                };
                match parse_footer(&mut file) {
                    Ok(members) => {
                        for (hash, offset, len) in members {
                            map.insert(
                                hash,
                                PackedLoc {
                                    pack: pack_hash,
                                    offset,
                                    len,
                                },
                            );
                        }
                    }
                    Err(_) => bad.push(path),
                }
            }
        }
        (map, bad)
    }
}

struct HashingWriter<W: Write> {
    inner: W,
    hasher: blake3::Hasher,
    written: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
