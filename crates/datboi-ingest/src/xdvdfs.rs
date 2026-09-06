//! Xbox disc layout reader (D111): parse the XDVDFS volume descriptor +
//! directory tree of an Xbox / Xbox 360 image into an EXACT coverage
//! map, and classify every non-data sector against the XGD1 mastering
//! filler stream, so the `xdvdfs-split/1` analyzer can decompose the
//! image into `assemble@1` pieces plus a zero-input filler recipe.
//!
//! The container is pure concatenation — volume descriptor at sector
//! 32 of the game partition, a binary-tree directory at absolute
//! sectors, files at absolute sector-aligned extents — so every piece
//! is a byte range and every recipe a builtin. Everything else on the
//! disc (the gaps between extents, the security-sector ranges the
//! drive cannot read, the layer pads, the video partition of a redump
//! image) is what this module classifies:
//!
//! * a sector that EQUALS the predicted next sector of the seed-era PRNG
//!   stream is a stream sector — the stream advances only over such
//!   sectors, in disc order, from game-partition sector 0;
//! * a zero run of exactly 4096 sectors under a known seed is a
//!   security-sector range: zero in every dump (unreadable), but the
//!   disc carries stream bytes there, so the stream advances past it —
//!   accepted only when the sector AFTER the run then predicts;
//! * any other uniform run is a fill; anything else is residue (a gap
//!   piece, or an inline literal when short).
//!
//! Nothing about redump geometry is hardcoded: partition bases are
//! probed by magic, security ranges and pads are recognized by shape,
//! and every stream sector is verified by equality before it is
//! claimed. A wrong guess costs residue, never a wrong claim — D4
//! replay is the proof either way. Structural anomalies (unparseable
//! tables, overlapping extents, tree cycles) are deterministic
//! conclusions ([`Refusal`]), never environmental errors (D81).

use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom};

use datboi_core::assemble::LITERAL_CAP;
use datboi_core::hash::Blake3;
use datboi_xf_xgd1_prng::{Prng, SECTOR_LEN, Workspace};

use crate::nds::{Piece, Region, read_at, u16_at, u32_at};

/// Bytes per disc sector.
pub const SECTOR: u64 = SECTOR_LEN as u64;
/// The volume descriptor lives at sector 32 of the game partition.
pub const VD_OFFSET: u64 = 0x10000;
pub const MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";
/// XGD1/XGD2 discs carry a mastering-tool signature sector right after
/// the volume descriptor.
pub const LAYOUT_SIG: &[u8; 24] = b"XBOX_DVD_LAYOUT_TOOL_SIG";
/// Game-partition byte offsets probed for the magic: a bare XISO, then
/// the redump image layouts (XGD1, XGD2, XGD2-hybrid, XGD3 — XboxKit's
/// table). Advisory: the magic decides, the table only says where to look.
pub const PARTITION_BASES: &[u64] = &[0, 0x1830_0000, 0x0FD9_0000, 0x89D8_0000, 0x0208_0000];
/// Every security-sector range is exactly this many sectors (16 of
/// them on XGD1, two on XGD2/3).
pub const SECURITY_RANGE_SECTORS: u64 = 4096;

/// Directory tables above this are a lie, not a big disc.
const MAX_DIR_TABLE: u64 = 16 << 20;
/// Entry cap across the whole tree (a retail disc has thousands).
const MAX_ENTRIES: usize = 1 << 20;
const MAX_DEPTH: usize = 64;
const ATTR_DIRECTORY: u8 = 0x10;
/// Sectors per read while classifying gaps.
const SCAN_SECTORS: usize = 64;

/// Structural refusals — deterministic conclusions about the bytes.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("not an xdvdfs image (no volume descriptor magic at a known partition base)")]
    NotXdvdfs,
    #[error("truncated: {0}")]
    Truncated(&'static str),
    #[error("directory table: {0}")]
    Directory(String),
    #[error("declared ranges overlap: {0} and {1}")]
    Overlap(String, String),
}

#[derive(Debug, thiserror::Error)]
pub enum XdvdfsError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Refused(#[from] Refusal),
}

/// The recovered filler stream: what the zero-input `fill` recipe claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filler {
    pub seed: u32,
    /// Stream sectors consumed (matched sectors + security ranges).
    pub sectors: u64,
    /// blake3 of the whole stream — the output identity of the recipe.
    pub hash: Blake3,
}

#[derive(Debug)]
pub struct Layout {
    pub total_len: u64,
    /// Byte offset of the game partition within the image.
    pub base: u64,
    /// In physical (coverage) order; regions reference these by index,
    /// and [`Region::Extern`] regions reference the filler stream
    /// (extern input 0).
    pub pieces: Vec<Piece>,
    pub regions: Vec<Region>,
    pub filler: Option<Filler>,
    pub file_count: usize,
    pub dir_count: usize,
    pub empty_files: usize,
    /// Mastering-tool build number from the signature sector, when
    /// present (advisory — recorded in the verdict, never a gate).
    pub layout_tool_build: Option<u16>,
    /// Sectors matched against the stream / consumed as zeroed security
    /// ranges / other uniform fill bytes.
    pub stream_sectors: u64,
    pub security_sectors: u64,
    pub fill_bytes: u64,
    /// Gap bytes that had to become pieces or literals.
    pub residual_bytes: u64,
    /// True when the entry count exceeded the piece cap and pieces are
    /// contiguous data runs instead of files.
    pub coalesced: bool,
}

/// One declared byte range: a file, a directory table, or the
/// descriptor literal.
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    start: u64,
    len: u64,
    literal: bool,
}

/// True if `head` (≥ 2048 bytes at a partition's `VD_OFFSET`) is an
/// XDVDFS volume descriptor: the magic opens and closes the sector.
#[must_use]
pub fn looks_like_vd(head: &[u8]) -> bool {
    head.len() >= SECTOR_LEN && &head[..20] == MAGIC && &head[0x7EC..0x800] == MAGIC
}

/// Probe the known partition bases for the volume descriptor.
///
/// # Errors
/// I/O only; `Ok(None)` when nothing sniffs.
pub fn find_partition<R: Read + Seek>(img: &mut R, total_len: u64) -> io::Result<Option<u64>> {
    for &base in PARTITION_BASES {
        let at = base + VD_OFFSET;
        if at + SECTOR > total_len {
            continue;
        }
        if looks_like_vd(&read_at(img, at, SECTOR_LEN)?) {
            return Ok(Some(base));
        }
    }
    Ok(None)
}

/// Parse the full layout: the directory tree, then one sequential pass
/// over the whole image classifying every byte outside the declared
/// extents. `max_pieces` caps the per-entry decomposition — above it,
/// pieces coalesce into contiguous data runs.
///
/// # Errors
/// [`XdvdfsError::Refused`] is a settled conclusion about the bytes;
/// [`XdvdfsError::Io`] is environmental.
pub fn parse_layout<R: Read + Seek>(img: &mut R, max_pieces: usize) -> Result<Layout, XdvdfsError> {
    let total_len = img.seek(SeekFrom::End(0))?;
    let base = find_partition(img, total_len)?.ok_or(Refusal::NotXdvdfs)?;
    let vd = read_at(img, base + VD_OFFSET, SECTOR_LEN)?;
    let root_sector = u32_at(&vd, 20);
    let root_size = u32_at(&vd, 24);

    // The signature sector (XGD1/XGD2) and its advisory build number:
    // two known field layouts (xblayout/xbpremaster pairs at +0x20, or
    // the later xbgamedisc record at +0x30 behind an all-zero +0x20).
    let mut has_sig = false;
    let mut layout_tool_build = None;
    if base + VD_OFFSET + 2 * SECTOR <= total_len {
        let sig = read_at(img, base + VD_OFFSET + SECTOR, SECTOR_LEN)?;
        if &sig[..24] == LAYOUT_SIG {
            has_sig = true;
            let build = if sig[0x20..0x28].iter().all(|&b| b == 0) {
                u16_at(&sig, 0x34)
            } else {
                u16_at(&sig, 0x24)
            };
            layout_tool_build = (build != 0).then_some(build);
        }
    }

    // Directory tree → entries (files + directory tables).
    let mut entries: Vec<Entry> = Vec::new();
    let mut file_count = 0usize;
    let mut dir_count = 0usize;
    let mut empty_files = 0usize;
    let mut seen_tables: HashSet<u64> = HashSet::new();
    let mut stack: Vec<(u64, u64, String, usize)> =
        vec![(root_sector, root_size, String::new(), 0)];
    while let Some((sector, size, path, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err(Refusal::Directory("tree deeper than 64 levels".into()).into());
        }
        if size == 0 {
            continue;
        }
        // A table's declared size is its BYTE length — sector-rounded on
        // some masters (Halo v1.09: 0x800), exact on others (Halo v1.02:
        // 88). The piece is the declared bytes; the slack classifies
        // like a file's tail.
        if size > MAX_DIR_TABLE {
            return Err(Refusal::Directory(format!(
                "table {} has implausible size {size}",
                display_path(&path)
            ))
            .into());
        }
        let start = base + sector * SECTOR;
        if start + size > total_len {
            return Err(Refusal::Truncated("directory table past the image").into());
        }
        if !seen_tables.insert(start) {
            return Err(Refusal::Directory(format!(
                "table {} referenced twice (not a tree)",
                display_path(&path)
            ))
            .into());
        }
        dir_count += 1;
        entries.push(Entry {
            name: format!("dir:{}", display_path(&path)),
            start,
            len: size,
            literal: false,
        });
        let table = read_at(img, start, usize::try_from(size).expect("capped"))?;
        walk_table(&table, &path, &mut |name, sector, len, is_dir| {
            if is_dir {
                stack.push((sector, len, name, depth + 1));
            } else if len == 0 {
                empty_files += 1;
            } else {
                let start = base + sector * SECTOR;
                if start + len > total_len {
                    return Err(Refusal::Truncated("file extent past the image"));
                }
                file_count += 1;
                entries.push(Entry {
                    name,
                    start,
                    len,
                    literal: false,
                });
            }
            Ok(())
        })?;
        if entries.len() > MAX_ENTRIES {
            return Err(Refusal::Directory("more entries than the tree cap".into()).into());
        }
    }

    // Order, dedupe exact aliases (two names over one extent are one
    // piece), refuse partial overlaps.
    entries.sort_by_key(|e| (e.start, e.len));
    entries.dedup_by(|b, a| a.start == b.start && a.len == b.len);
    check_overlaps(&entries)?;

    // Above the piece cap, pieces are contiguous data runs (sector
    // aligned) — recipe volume bounded, filler classification unchanged.
    let coalesced = entries.len() > max_pieces;
    let mut declared: Vec<Entry> = if coalesced {
        let mut runs: Vec<Entry> = Vec::new();
        for e in &entries {
            let end = (e.start + e.len).div_ceil(SECTOR) * SECTOR;
            let start = e.start / SECTOR * SECTOR;
            match runs.last_mut() {
                Some(last) if start <= last.start + last.len => {
                    last.len = end.max(last.start + last.len) - last.start;
                }
                _ => runs.push(Entry {
                    name: format!("extent@0x{start:09x}"),
                    start,
                    len: end - start,
                    literal: false,
                }),
            }
        }
        runs
    } else {
        entries
    };

    // The descriptor sector(s) are data too — an inlined literal in the
    // rebuild (unique per disc: root pointer, timestamp, build stamps).
    declared.push(Entry {
        name: "volume descriptor".into(),
        start: base + VD_OFFSET,
        len: if has_sig { 2 * SECTOR } else { SECTOR },
        literal: true,
    });
    declared.sort_by_key(|e| e.start);
    check_overlaps(&declared)?;

    // Seed recovery from game-partition sector 0 — the stream's origin
    // on every seed-era disc. Zeroed or data there means no stream.
    let mut filler_seed = None;
    if base + SECTOR <= total_len && !covers(&declared, base) {
        let first: [u8; SECTOR_LEN] = read_at(img, base, SECTOR_LEN)?
            .try_into()
            .expect("sector-sized");
        if first.iter().any(|&b| b != first[0]) {
            let mut ws = Box::new(Workspace::new());
            filler_seed = ws.recover_default(&first, 0);
        }
    }

    // Coverage walk: declared ranges in order, every gap classified in
    // one sequential pass with the stream predictor.
    let mut cls = Classifier::new(img, filler_seed, base);
    let mut pieces: Vec<Piece> = Vec::new();
    let mut regions: Vec<Region> = Vec::new();
    let mut cursor = 0u64;
    for entry in declared {
        cls.classify(cursor, entry.start - cursor, &mut regions, &mut pieces)?;
        if entry.literal {
            regions.push(Region::Literal {
                start: entry.start,
                len: entry.len,
            });
        } else {
            regions.push(Region::Piece(pieces.len()));
            pieces.push(Piece {
                name: entry.name,
                start: entry.start,
                len: entry.len,
            });
        }
        cursor = entry.start + entry.len;
    }
    cls.classify(cursor, total_len - cursor, &mut regions, &mut pieces)?;

    let filler = cls.finish();
    Ok(Layout {
        total_len,
        base,
        pieces,
        regions,
        filler,
        file_count,
        dir_count,
        empty_files,
        layout_tool_build,
        stream_sectors: cls.stream_sectors,
        security_sectors: cls.security_sectors,
        fill_bytes: cls.fill_bytes,
        residual_bytes: cls.residual_bytes,
        coalesced,
    })
}

fn display_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_owned()
    } else {
        path.to_owned()
    }
}

fn covers(entries: &[Entry], at: u64) -> bool {
    entries
        .iter()
        .any(|e| e.start <= at && at < e.start + e.len)
}

fn check_overlaps(sorted: &[Entry]) -> Result<(), Refusal> {
    for pair in sorted.windows(2) {
        if pair[1].start < pair[0].start + pair[0].len {
            return Err(Refusal::Overlap(pair[0].name.clone(), pair[1].name.clone()));
        }
    }
    Ok(())
}

/// Walk one directory table's binary tree. Offsets are in 4-byte units
/// from the table start; `0xFFFF` in both links at offset 0 marks an
/// empty table; entries never straddle a sector (the table pads with
/// 0xFF), which the link-following walk never has to know.
fn walk_table(
    table: &[u8],
    path: &str,
    visit: &mut dyn FnMut(String, u64, u64, bool) -> Result<(), Refusal>,
) -> Result<(), Refusal> {
    let refuse = |what: String| Refusal::Directory(format!("{}: {what}", display_path(path)));
    let mut seen: HashSet<usize> = HashSet::new();
    let mut todo: Vec<usize> = vec![0];
    while let Some(off) = todo.pop() {
        if !seen.insert(off) {
            return Err(refuse(format!("entry at {off} linked twice")));
        }
        if off + 14 > table.len() {
            return Err(refuse(format!("entry at {off} truncated")));
        }
        let left = u16_at(table, off);
        let right = u16_at(table, off + 2);
        if left == 0xFFFF && right == 0xFFFF {
            if off == 0 {
                return Ok(()); // empty directory
            }
            return Err(refuse(format!("sentinel entry linked at {off}")));
        }
        let sector = u32_at(table, off + 4);
        let len = u32_at(table, off + 8);
        let attr = table[off + 12];
        let name_len = usize::from(table[off + 13]);
        if name_len == 0 || off + 14 + name_len > table.len() {
            return Err(refuse(format!("entry at {off} has a bad name length")));
        }
        // Lossy on purpose: claims need a stable label, not a faithful
        // filesystem round-trip (the zip precedent).
        let name = String::from_utf8_lossy(&table[off + 14..off + 14 + name_len]);
        visit(
            format!("{path}/{name}"),
            sector,
            len,
            attr & ATTR_DIRECTORY != 0,
        )?;
        for link in [left, right] {
            if link != 0 {
                let child = usize::from(link) * 4;
                if child >= table.len() {
                    return Err(refuse(format!("link {link} past the table")));
                }
                todo.push(child);
            }
        }
        if seen.len() > MAX_ENTRIES {
            return Err(refuse("more entries than the tree cap".into()));
        }
    }
    Ok(())
}

/// A run of like-classified bytes, pending emission as one region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    /// Stream sectors `[first, first + count)`.
    Stream { first: u64, count: u64 },
    /// Uniform bytes at absolute `start`.
    Fill { byte: u8, start: u64, len: u64 },
    /// Residue bytes at absolute `start`.
    Residue { start: u64, len: u64 },
}

/// The gap classifier: streams the image sequentially through the gaps
/// between declared ranges, predicting the filler stream and hashing
/// it as it goes.
struct Classifier<'r, R> {
    img: &'r mut R,
    seed: Option<u32>,
    base: u64,
    /// Stream sectors consumed so far; `prng` sits at this position.
    stream_pos: u64,
    prng: Option<Prng>,
    hasher: blake3::Hasher,
    pending: Option<Run>,
    stream_sectors: u64,
    security_sectors: u64,
    fill_bytes: u64,
    residual_bytes: u64,
    buf: Vec<u8>,
}

impl<'r, R: Read + Seek> Classifier<'r, R> {
    fn new(img: &'r mut R, seed: Option<u32>, base: u64) -> Self {
        Self {
            img,
            seed,
            base,
            stream_pos: 0,
            prng: seed.map(|s| Prng::new(s, 0)),
            hasher: blake3::Hasher::new(),
            pending: None,
            stream_sectors: 0,
            security_sectors: 0,
            fill_bytes: 0,
            residual_bytes: 0,
            buf: vec![0u8; SECTOR_LEN * SCAN_SECTORS],
        }
    }

    fn finish(&mut self) -> Option<Filler> {
        let seed = self.seed?;
        (self.stream_pos > 0).then(|| Filler {
            seed,
            sectors: self.stream_pos,
            hash: Blake3(*self.hasher.finalize().as_bytes()),
        })
    }

    /// Classify `[start, start + len)` — a gap between declared
    /// ranges — appending regions (and residue pieces). Whole aligned
    /// sectors go through the stream predictor; partial sectors (the
    /// slack tail of a file, or a gap ending mid-sector) are fill or
    /// residue bytes.
    fn classify(
        &mut self,
        start: u64,
        len: u64,
        regions: &mut Vec<Region>,
        pieces: &mut Vec<Piece>,
    ) -> Result<(), XdvdfsError> {
        if len == 0 {
            return Ok(());
        }
        self.img.seek(SeekFrom::Start(start))?;
        let mut buf = std::mem::take(&mut self.buf);
        let end = start + len;
        let mut pos = start;
        while pos < end {
            let aligned = pos.is_multiple_of(SECTOR) && end - pos >= SECTOR;
            let take = if aligned {
                ((end - pos) / SECTOR * SECTOR).min(buf.len() as u64)
            } else {
                (SECTOR - pos % SECTOR).min(end - pos)
            };
            let n = usize::try_from(take).expect("bounded by buf");
            self.img.read_exact(&mut buf[..n])?;
            if aligned {
                for (i, sector) in buf[..n].chunks_exact(SECTOR_LEN).enumerate() {
                    let abs = pos + (i * SECTOR_LEN) as u64;
                    let next = self.classify_sector(sector, abs);
                    self.push(next, regions, pieces);
                }
            } else {
                let bytes = &buf[..n];
                let next = if bytes.iter().all(|&b| b == bytes[0]) {
                    Run::Fill {
                        byte: bytes[0],
                        start: pos,
                        len: take,
                    }
                } else {
                    Run::Residue {
                        start: pos,
                        len: take,
                    }
                };
                self.push(next, regions, pieces);
            }
            pos += take;
        }
        self.buf = buf;
        self.flush(regions, pieces);
        Ok(())
    }

    /// Predict-and-compare for one aligned sector at absolute `abs`.
    fn classify_sector(&mut self, sector: &[u8], abs: u64) -> Run {
        if abs >= self.base
            && let (Some(seed), Some(prng)) = (self.seed, self.prng)
        {
            let mut generated = [0u8; SECTOR_LEN];
            // A pending zero run of exactly one security range: the
            // stream may have advanced across it. Accept that reading
            // only if this sector then predicts from the jumped position.
            if let Some(Run::Fill {
                byte: 0,
                start,
                len,
            }) = self.pending
                && start >= self.base
                && start.is_multiple_of(SECTOR)
                && len == SECURITY_RANGE_SECTORS * SECTOR
                && let Ok(jumped_pos) = u32::try_from(self.stream_pos + SECURITY_RANGE_SECTORS)
            {
                let mut g = Prng::new(seed, jumped_pos);
                g.fill_sector(&mut generated);
                if generated == sector {
                    // Commit: hash the unread range in stream order.
                    let mut skipped =
                        Prng::new(seed, u32::try_from(self.stream_pos).expect("< jumped"));
                    let mut s = [0u8; SECTOR_LEN];
                    for _ in 0..SECURITY_RANGE_SECTORS {
                        skipped.fill_sector(&mut s);
                        self.hasher.update(&s);
                    }
                    self.stream_pos += SECURITY_RANGE_SECTORS;
                    self.security_sectors += SECURITY_RANGE_SECTORS;
                    return self.matched(g, &generated);
                }
            }
            let mut g = prng;
            g.fill_sector(&mut generated);
            if generated == sector {
                return self.matched(g, &generated);
            }
        }
        if sector.iter().all(|&b| b == sector[0]) {
            return Run::Fill {
                byte: sector[0],
                start: abs,
                len: SECTOR,
            };
        }
        Run::Residue {
            start: abs,
            len: SECTOR,
        }
    }

    fn matched(&mut self, after: Prng, generated: &[u8; SECTOR_LEN]) -> Run {
        self.hasher.update(generated);
        self.prng = Some(after);
        let ix = self.stream_pos;
        self.stream_pos += 1;
        self.stream_sectors += 1;
        Run::Stream {
            first: ix,
            count: 1,
        }
    }

    /// Merge `next` into the pending run or flush and start anew.
    fn push(&mut self, next: Run, regions: &mut Vec<Region>, pieces: &mut Vec<Piece>) {
        match (self.pending.as_mut(), next) {
            (Some(Run::Stream { first, count }), Run::Stream { first: f, count: c })
                if *first + *count == f =>
            {
                *count += c;
            }
            (
                Some(Run::Fill { byte, start, len }),
                Run::Fill {
                    byte: b,
                    start: s,
                    len: l,
                },
            ) if *byte == b && *start + *len == s => {
                *len += l;
            }
            (Some(Run::Residue { start, len }), Run::Residue { start: s, len: l })
                if *start + *len == s =>
            {
                *len += l;
            }
            _ => {
                self.flush(regions, pieces);
                self.pending = Some(next);
            }
        }
    }

    fn flush(&mut self, regions: &mut Vec<Region>, pieces: &mut Vec<Piece>) {
        let Some(run) = self.pending.take() else {
            return;
        };
        match run {
            Run::Stream { first, count } => regions.push(Region::Extern {
                input: 0,
                offset: first * SECTOR,
                len: count * SECTOR,
            }),
            Run::Fill { byte, len, .. } => {
                self.fill_bytes += len;
                regions.push(Region::Fill { byte, len });
            }
            Run::Residue { start, len } => {
                self.residual_bytes += len;
                if len <= LITERAL_CAP as u64 {
                    regions.push(Region::Literal { start, len });
                } else {
                    regions.push(Region::Piece(pieces.len()));
                    pieces.push(Piece {
                        name: format!("gap@0x{start:09x}"),
                        start,
                        len,
                    });
                }
            }
        }
    }
}

/// Synthetic image builder for the gates (the D111 sweep test in this
/// crate and the executor's shrink test): a bare XISO at base 0 with
/// real seed-era filler, a security-sector-shaped zero run the stream
/// skips, slack tails, a zero pad, and a residue tail — every shape
/// the classifier distinguishes, in one ~8.5 MiB image.
#[doc(hidden)]
pub mod synth {
    use super::{LAYOUT_SIG, MAGIC, SECTOR, SECURITY_RANGE_SECTORS};
    use datboi_xf_xgd1_prng::{Prng, SECTOR_LEN};

    /// How the gaps are filled.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Filler {
        /// The seed-era PRNG stream from this seed.
        Seed(u32),
        /// Unrecoverable bytes (the rc4-era shape).
        Random,
    }

    /// One built image and what the analyzer should find in it.
    pub struct Image {
        pub bytes: Vec<u8>,
        /// (path, bytes) for every non-empty file, in disc order.
        pub files: Vec<(String, Vec<u8>)>,
        /// Stream sectors a seed-era image consumes (matched + the
        /// security range).
        pub stream_sectors: u64,
        /// Sector index of the residue tail.
        pub residue_sector: u64,
    }

    fn pattern(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    fn entry(out: &mut Vec<u8>, right: u16, sector: u32, size: u32, attr: u8, name: &str) {
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&right.to_le_bytes());
        out.extend_from_slice(&sector.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.push(attr);
        out.push(u8::try_from(name.len()).expect("short name"));
        out.extend_from_slice(name.as_bytes());
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }

    fn table(entries: &[(u32, u32, u8, &str)]) -> Vec<u8> {
        // A right-linked chain: each entry's right link points at the
        // next (offsets in 4-byte units), the last has none.
        let mut out = Vec::new();
        for (i, (sector, size, attr, name)) in entries.iter().enumerate() {
            let this_len = (14 + name.len()).div_ceil(4) * 4;
            let right = if i + 1 < entries.len() {
                u16::try_from((out.len() + this_len) / 4).expect("small table")
            } else {
                0
            };
            entry(&mut out, right, *sector, *size, *attr, name);
        }
        assert!(out.len() <= SECTOR_LEN);
        out.resize(SECTOR_LEN, 0xFF);
        out
    }

    /// The sub table's declared size: its exact entry bytes (two 20-byte
    /// entries), the Halo v1.02 shape — the rest of its sector is 0xFF pad.
    pub const SUB_TABLE_LEN: u32 = 40;

    /// Build the fixture. `trimmed` cuts the image after the last file
    /// extent (XboxKit's `--trim`), dropping the security range, pad,
    /// and residue tail.
    #[must_use]
    pub fn image(filler: Filler, trimmed: bool) -> Image {
        let mut stream = match filler {
            Filler::Seed(seed) => Some(Prng::new(seed, 0)),
            Filler::Random => None,
        };
        let mut stream_sectors = 0u64;
        let mut junk_state = 0x9E37_79B9_7F4A_7C15u64;
        fn fill(
            out: &mut Vec<u8>,
            stream: &mut Option<Prng>,
            stream_sectors: &mut u64,
            junk_state: &mut u64,
            count: u64,
        ) {
            for _ in 0..count {
                let mut s = [0u8; SECTOR_LEN];
                match stream.as_mut() {
                    Some(g) => {
                        g.fill_sector(&mut s);
                        *stream_sectors += 1;
                    }
                    None => {
                        *junk_state = junk_state
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1);
                        s.copy_from_slice(&pattern(SECTOR_LEN, *junk_state));
                    }
                }
                out.extend_from_slice(&s);
            }
        }

        let a = pattern(3000, 0xA);
        let b = pattern(2 * SECTOR_LEN, 0xB);
        let c = pattern(SECTOR_LEN + 1, 0xC);
        let mut out = Vec::new();

        // 0..32: filler (stream 0..31).
        fill(
            &mut out,
            &mut stream,
            &mut stream_sectors,
            &mut junk_state,
            32,
        );
        // 32: volume descriptor → root table at sector 34.
        let mut vd = vec![0u8; SECTOR_LEN];
        vd[..20].copy_from_slice(MAGIC);
        vd[20..24].copy_from_slice(&34u32.to_le_bytes());
        vd[24..28].copy_from_slice(&(SECTOR as u32).to_le_bytes());
        vd[28..36].copy_from_slice(&0x01C3_AAA6_30E4_A610u64.to_le_bytes());
        vd[0x7EC..].copy_from_slice(MAGIC);
        out.extend_from_slice(&vd);
        // 33: layout-tool signature with an xblayout-style build (3926).
        let mut sig = vec![0u8; SECTOR_LEN];
        sig[..24].copy_from_slice(LAYOUT_SIG);
        sig[0x20..0x28].copy_from_slice(&[1, 0, 0, 0, 0x56, 0x0F, 1, 0]);
        out.extend_from_slice(&sig);
        // 34: root table: a.bin @36 (3000 B), sub @40, empty.bin (0 B).
        out.extend_from_slice(&table(&[
            (36, 3000, 0x20, "a.bin"),
            (40, SUB_TABLE_LEN, 0x10, "sub"),
            (0, 0, 0x20, "empty.bin"),
        ]));
        // 35: filler (stream 32).
        fill(
            &mut out,
            &mut stream,
            &mut stream_sectors,
            &mut junk_state,
            1,
        );
        // 36..38: a.bin + zero slack.
        out.extend_from_slice(&a);
        out.resize(38 * SECTOR_LEN, 0);
        // 38..40: filler (stream 33, 34).
        fill(
            &mut out,
            &mut stream,
            &mut stream_sectors,
            &mut junk_state,
            2,
        );
        // 40: sub table: b.bin @41 (2 sectors), c.bin @43 (1 sector + 1 B);
        // declared at its exact 40 bytes, 0xFF-padded to the sector.
        let sub = table(&[
            (41, 2 * SECTOR as u32, 0x20, "b.bin"),
            (43, SECTOR as u32 + 1, 0x20, "c.bin"),
        ]);
        assert_eq!(
            sub[SUB_TABLE_LEN as usize], 0xFF,
            "declared length is the entry bytes"
        );
        out.extend_from_slice(&sub);
        // 41..43: b.bin.
        out.extend_from_slice(&b);
        // 43..45: c.bin + zero slack.
        out.extend_from_slice(&c);
        out.resize(45 * SECTOR_LEN, 0);
        let files = vec![
            ("/a.bin".to_owned(), a),
            ("/sub/b.bin".to_owned(), b),
            ("/sub/c.bin".to_owned(), c),
        ];
        if trimmed {
            return Image {
                bytes: out,
                files,
                stream_sectors,
                residue_sector: 0,
            };
        }
        // 45..49: filler (stream 35..38).
        fill(
            &mut out,
            &mut stream,
            &mut stream_sectors,
            &mut junk_state,
            4,
        );
        // 49..4145: a security-sector range — zero on disc, but the
        // stream advanced across it.
        out.resize(
            out.len() + usize::try_from(SECURITY_RANGE_SECTORS * SECTOR).expect("small"),
            0,
        );
        if let Some(g) = stream.as_mut() {
            let mut s = [0u8; SECTOR_LEN];
            for _ in 0..SECURITY_RANGE_SECTORS {
                g.fill_sector(&mut s);
            }
            stream_sectors += SECURITY_RANGE_SECTORS;
        }
        // 4145..4149: filler (stream 4135..4138).
        fill(
            &mut out,
            &mut stream,
            &mut stream_sectors,
            &mut junk_state,
            4,
        );
        // 4149..4152: zero pad (never consumed).
        out.resize(out.len() + 3 * SECTOR_LEN, 0);
        // 4152..4155: residue (the layer-1 video shape): 3 sectors, over
        // the literal cap so it becomes a gap piece.
        let residue_sector = (out.len() / SECTOR_LEN) as u64;
        out.extend_from_slice(&pattern(3 * SECTOR_LEN, 0xD));
        Image {
            bytes: out,
            files,
            stream_sectors,
            residue_sector,
        }
    }
}
