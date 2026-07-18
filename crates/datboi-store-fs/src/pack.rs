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
//! Format (little-endian throughout, D105):
//!
//! ```text
//! [member 0 bytes][member 1 bytes]…      back-to-back from 0, coverage order
//! [outboard section]                     member-rooted obao4 trees, member order
//! [footer: b"datboi/pack/1\n"
//!          u32 member count
//!          per member: 32-byte blake3, u64 offset, u64 len]
//! [32-byte blake3(footer)][u64 footer_len][b"DBOIPACK"]
//! ```
//!
//! The trailer locates AND authenticates the footer from the end in
//! one small tail read: a footer that fails its trailer hash refuses
//! the pack at open — a plausible-but-wrong member table can never
//! mis-slice a member through the plain-read path (D105). The
//! outboard section is DERIVED, not described: trees sit in member
//! order starting at the last member's end, each exactly
//! `outboard_size(len)` bytes (zero for ≤ 16 KiB members — absence IS
//! the empty sidecar, the loose-sidecar rule), and the parser
//! enforces that members are contiguous from zero and that data +
//! section + footer + trailer tile the file exactly. Redundancy that
//! could disagree with reality doesn't exist to disagree.
//!
//! Each tree is a BYPRODUCT of the write's own verification: the bao
//! root IS the member's blake3 identity (the D52 golden), so
//! `put_pack` proves each member and produces its outboard in the
//! same pass. Packed members therefore never need loose `.obao4`
//! sidecars, and recovery (`scan_packs`) sees the trees by
//! construction — no lazy blessing backstop.

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

/// A written pack's identity + its parsed-shape member entries.
type SealedPack = (Blake3, Vec<MemberEntry>);

/// One member as the footer + derived outboard section describe it —
/// what `parse_footer` returns and `put_pack` records.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MemberEntry {
    pub hash: Blake3,
    pub offset: u64,
    pub len: u64,
    /// This member's obao4 tree window in the outboard section,
    /// derived (never stored) per D105. `obao_len == 0` ⇔ the member
    /// is ≤ one chunk group and its outboard is empty.
    pub obao_offset: u64,
    pub obao_len: u64,
}

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
/// Trailer: 32-byte blake3(footer) + u64 footer length + 8-byte magic
/// (D105 — the hash is what makes open-time footer trust cheap).
const TRAILER_LEN: u64 = 48;
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
    /// The member's obao4 tree window in the pack's outboard section
    /// (D105); `obao_len == 0` for ≤ one-chunk-group members.
    pub obao_offset: u64,
    pub obao_len: u64,
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

/// Parse a pack file's footer into its member table, authenticate it
/// against the trailer hash, and derive the outboard section layout
/// (D105).
///
/// # Errors
/// A malformed footer — wrong magic, truncated table, a footer that
/// fails its trailer hash, rows that aren't contiguous from zero, or a
/// derived section that doesn't tile the file — refuses the pack
/// whole; members it held stay resolvable only if another copy exists.
pub(crate) fn parse_footer(file: &mut File) -> Result<Vec<MemberEntry>, String> {
    let total = file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    if total < TRAILER_LEN {
        return Err("shorter than the trailer".into());
    }
    file.seek(SeekFrom::End(
        -i64::try_from(TRAILER_LEN).expect("trailer fits i64"),
    ))
    .map_err(|e| e.to_string())?;
    let mut trailer = [0u8; 48];
    file.read_exact(&mut trailer).map_err(|e| e.to_string())?;
    if &trailer[40..] != PACK_TRAILER {
        return Err("missing trailer magic".into());
    }
    let footer_hash = Blake3(trailer[..32].try_into().expect("thirty-two bytes"));
    let footer_len = u64::from_le_bytes(trailer[32..40].try_into().expect("eight bytes"));
    let footer_start = total
        .checked_sub(TRAILER_LEN)
        .and_then(|v| v.checked_sub(footer_len))
        .ok_or("footer length exceeds the file")?;
    file.seek(SeekFrom::Start(footer_start))
        .map_err(|e| e.to_string())?;
    let mut footer = vec![0u8; usize::try_from(footer_len).map_err(|e| e.to_string())?];
    file.read_exact(&mut footer).map_err(|e| e.to_string())?;
    // D105: not one byte of the table is trusted before the footer
    // matches the hash the trailer recorded.
    if Blake3::compute(&footer) != footer_hash {
        return Err("footer does not match its trailer hash".into());
    }
    let table = footer
        .strip_prefix(PACK_MAGIC)
        .ok_or("missing footer magic")?;
    if table.len() < 4 {
        return Err("truncated member count".into());
    }
    let count = u32::from_le_bytes(table[..4].try_into().expect("four bytes")) as usize;
    if count == 0 {
        // put_pack refuses empty packs; an empty table is malformed.
        return Err("empty member table".into());
    }
    let rows = &table[4..];
    if rows.len() != count * MEMBER_ROW {
        return Err("member table length disagrees with the count".into());
    }
    // Members are contiguous from zero (write order) — enforced so the
    // derived section layout below cannot be fooled by a plausible
    // offset, and so offsets stay ascending by construction.
    let mut members = Vec::with_capacity(count);
    let mut expect = 0u64;
    for row in rows.chunks_exact(MEMBER_ROW) {
        let hash = Blake3(row[..32].try_into().expect("thirty-two bytes"));
        let offset = u64::from_le_bytes(row[32..40].try_into().expect("eight bytes"));
        let len = u64::from_le_bytes(row[40..48].try_into().expect("eight bytes"));
        if offset != expect {
            return Err("member rows are not contiguous from zero".into());
        }
        expect = offset
            .checked_add(len)
            .ok_or("member length overflows the pack")?;
        members.push(MemberEntry {
            hash,
            offset,
            len,
            obao_offset: 0,
            obao_len: 0,
        });
    }
    // Derive the outboard section (D105): trees in member order at the
    // last member's end, each exactly outboard_size(len) bytes — and
    // the whole thing must tile the gap up to the footer exactly.
    let mut cursor = expect;
    for member in &mut members {
        member.obao_len = crate::obao::outboard_size(member.len);
        member.obao_offset = cursor;
        cursor = cursor
            .checked_add(member.obao_len)
            .ok_or("outboard section overflows the pack")?;
    }
    if cursor != footer_start {
        return Err("outboard section does not tile the file".into());
    }
    Ok(members)
}

pub(crate) fn encode_footer(members: &[MemberEntry]) -> Vec<u8> {
    let mut footer = Vec::with_capacity(PACK_MAGIC.len() + 4 + members.len() * MEMBER_ROW);
    footer.extend_from_slice(PACK_MAGIC);
    footer.extend_from_slice(
        &u32::try_from(members.len())
            .expect("member count fits u32")
            .to_le_bytes(),
    );
    // Rows carry (hash, offset, len) only — the outboard section is
    // derived from them at parse, never described here (D105).
    for member in members {
        footer.extend_from_slice(&member.hash.0);
        footer.extend_from_slice(&member.offset.to_le_bytes());
        footer.extend_from_slice(&member.len.to_le_bytes());
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
                for entry in table {
                    packs.insert(
                        entry.hash,
                        PackedLoc {
                            pack: pack_hash,
                            offset: entry.offset,
                            len: entry.len,
                            obao_offset: entry.obao_offset,
                            obao_len: entry.obao_len,
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
        // The outboard section spools to a staging file, not RAM: it's
        // ~0.4% of the data, which is real memory at disc-image pack
        // scale (D105).
        let spool_path = self.staging_path("pack-obao");
        let result = self.write_pack_spooled(temp, &spool_path, members, open);
        let _ = fs::remove_file(&spool_path);
        result
    }

    fn write_pack_spooled<'r>(
        &self,
        temp: &PathBuf,
        spool_path: &PathBuf,
        members: &[PackMember],
        open: &mut impl FnMut(usize) -> io::Result<Box<dyn Read + 'r>>,
    ) -> Result<SealedPack, StoreError> {
        let file = File::create(temp).map_err(|e| StoreError::io(temp, e))?;
        let mut out = HashingWriter {
            inner: io::BufWriter::new(file),
            hasher: blake3::Hasher::new(),
            written: 0,
        };
        let mut spool = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(spool_path)
            .map_err(|e| StoreError::io(spool_path, e))?;
        let mut rows: Vec<(Blake3, u64, u64)> = Vec::with_capacity(members.len());
        for (ix, member) in members.iter().enumerate() {
            let offset = out.written;
            let mut reader = open(ix).map_err(|e| StoreError::io(temp, e))?;
            // The member's obao tree is a byproduct of the verification
            // this loop always did: the bao root IS the member's blake3
            // (D52 golden), so one tee'd pass writes the bytes, proves
            // the identity, and yields the outboard (D105).
            let (root, sidecar) = crate::obao::compute(
                TeeRead {
                    inner: &mut reader,
                    out: &mut out,
                },
                member.len,
            )
            .map_err(|e| match e {
                crate::obao::ObaoError::Io(err) => StoreError::io(temp, err),
                other => StoreError::Obao {
                    path: temp.clone(),
                    source: other,
                },
            })?;
            // A stream longer than declared refuses the pack even when
            // the first `len` bytes hash correctly — the declared
            // identity is (hash, len) exactly.
            let extra =
                io::copy(&mut reader, &mut io::sink()).map_err(|e| StoreError::io(temp, e))?;
            if root != member.hash || extra > 0 {
                return Err(StoreError::PackMemberMismatch {
                    expected: member.hash,
                    got: root,
                    expected_len: member.len,
                    got_len: member.len.saturating_add(extra),
                });
            }
            spool
                .write_all(&sidecar)
                .map_err(|e| StoreError::io(spool_path, e))?;
            rows.push((member.hash, offset, member.len));
        }
        // Replay the spooled outboard section into the hashed stream,
        // then derive each tree's window exactly as the parser will.
        let section_start = out.written;
        spool
            .seek(SeekFrom::Start(0))
            .map_err(|e| StoreError::io(spool_path, e))?;
        io::copy(&mut io::BufReader::new(&mut spool), &mut out)
            .map_err(|e| StoreError::io(temp, e))?;
        let mut cursor = section_start;
        let table: Vec<MemberEntry> = rows
            .into_iter()
            .map(|(hash, offset, len)| {
                let obao_len = crate::obao::outboard_size(len);
                let entry = MemberEntry {
                    hash,
                    offset,
                    len,
                    obao_offset: cursor,
                    obao_len,
                };
                cursor += obao_len;
                entry
            })
            .collect();
        debug_assert_eq!(cursor, out.written, "section tiles by construction");
        let footer = encode_footer(&table);
        out.write_all(&footer)
            .map_err(|e| StoreError::io(temp, e))?;
        out.write_all(&Blake3::compute(&footer).0)
            .map_err(|e| StoreError::io(temp, e))?;
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
        let members = parse_footer(&mut file).map_err(|detail| StoreError::PackFooter {
            path: path.clone(),
            detail,
        })?;
        // Coverage order = write order = read order — the parser
        // enforces contiguity from zero, so entries arrive ascending;
        // drive per-member hashers as the single forward pass crosses
        // their spans.
        file.seek(SeekFrom::Start(0))
            .map_err(|e| StoreError::io(&path, e))?;
        let mut whole = blake3::Hasher::new();
        let mut hashers: Vec<AliasHasher> = members.iter().map(|_| AliasHasher::new()).collect();
        let mut reader = io::BufReader::new(&mut file);
        let mut buf = vec![0u8; 64 * 1024];
        let mut pos = 0u64;
        let mut cur = 0usize;
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| StoreError::io(&path, e))?;
            if n == 0 {
                break;
            }
            whole.update(&buf[..n]);
            let (chunk_start, chunk_end) = (pos, pos + n as u64);
            // Fan the buffer out over the members it overlaps. Outboard-
            // section, footer, and trailer bytes past the last member
            // fall through to `whole` only. At most one member straddles
            // any buffer boundary.
            while cur < members.len() {
                let (mstart, mend) = (members[cur].offset, members[cur].offset + members[cur].len);
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
            .map(|(member, hasher)| {
                let aliases = hasher.finalize();
                PackMemberScrub {
                    hash: member.hash,
                    len: member.len,
                    // Trusted for back-fill on its own merits: the slice's
                    // own bytes hashed to its identity.
                    aliases: (aliases.blake3 == member.hash).then_some(aliases),
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
        let mut survivors: Vec<MemberEntry> = Vec::new();
        let mut dropped: Vec<Blake3> = Vec::new();
        let mut bytes_freed = 0u64;
        for member in &members {
            if drop.contains(&member.hash) {
                dropped.push(member.hash);
                bytes_freed += member.len;
            } else {
                survivors.push(*member);
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
                for member in &members {
                    map.remove(&member.hash);
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
        // Rewrite the survivors (coverage order — parse guarantees
        // ascending) into a fresh pack, streaming each straight out of
        // the old pack's window — which re-verifies every survivor
        // against its hash and regrows its outboard tree as it lands.
        let pack_members: Vec<PackMember> = survivors
            .iter()
            .map(|m| PackMember {
                hash: m.hash,
                len: m.len,
            })
            .collect();
        let old = File::open(&old_path).map_err(|e| StoreError::io(&old_path, e))?;
        let new_hash = self.put_pack(&pack_members, |ix| {
            let m = survivors[ix];
            Ok(Box::new(Blob::packed(old.try_clone()?, m.offset, m.len)?))
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
                        for member in members {
                            map.insert(
                                member.hash,
                                PackedLoc {
                                    pack: pack_hash,
                                    offset: member.offset,
                                    len: member.len,
                                    obao_offset: member.obao_offset,
                                    obao_len: member.obao_len,
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

/// Reads from `inner`, mirroring every byte into `out` — how the pack
/// write stores, hashes, and grows a member's outboard in one pass.
struct TeeRead<'a, R: Read, W: Write> {
    inner: R,
    out: &'a mut W,
}

impl<R: Read, W: Write> Read for TeeRead<'_, R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.out.write_all(&buf[..n])?;
        Ok(n)
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
