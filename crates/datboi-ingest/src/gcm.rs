//! GameCube disc layout reader (D115): parse the boot header, the
//! apploader, the DOL, and the file system table of a GameCube image
//! into an EXACT coverage map, and classify every unused byte against
//! the mastering junk generator, so the `gcm-split/1` analyzer can
//! decompose the image into `assemble@1` pieces plus a zero-input junk
//! recipe — the D111 shape on a disc whose filler is even simpler.
//!
//! The container is pure concatenation — boot.bin + bi2.bin at 0, the
//! apploader at 0x2440, the DOL and the FST where the boot block points,
//! files at absolute FST offsets — so every piece is a byte range and
//! every recipe a builtin. Everything else on the disc (the gaps
//! between extents, the pad out to 1.46 GB) is what this module
//! classifies:
//!
//! * a byte that EQUALS the junk generator's output for that disc
//!   position is junk — the generator is a pure function of (game ID,
//!   disc number, position), reseeded every 32 KiB, so no stream state
//!   crosses a data extent and a matched run is a range of the junk
//!   stream at the SAME offsets;
//! * any other uniform run is a fill; anything else is residue (a gap
//!   piece, or an inline literal when short).
//!
//! Nothing about the disc's size is assumed (a trimmed image is walked
//! as far as it goes; an FST extent past the end is a refusal), and
//! every junk byte is verified by equality before it is claimed. A
//! wrong guess costs residue, never a wrong claim — D4 replay is the
//! proof either way. Structural anomalies are deterministic
//! conclusions ([`Refusal`]), never environmental errors (D81).

use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom};

use datboi_core::assemble::LITERAL_CAP;
use datboi_core::hash::Blake3;
use datboi_xf_gc_junk::{Lfg, SECTOR_LEN, fill_at};

use crate::nds::{Piece, Region, read_at};

/// The junk generator's reseed pitch.
pub const SECTOR: u64 = SECTOR_LEN as u64;
/// Magic at 0x1C of every GameCube disc.
pub const MAGIC: [u8; 4] = [0xC2, 0x33, 0x9F, 0x3D];
/// Magic at 0x18 of a Wii disc — a different analyzer's business.
pub const WII_MAGIC: [u8; 4] = [0x5D, 0x1C, 0x9E, 0xA3];
/// boot.bin + bi2.bin.
pub const BOOT_LEN: u64 = 0x440;
pub const BI2_LEN: u64 = 0x2000;
/// The apploader's fixed position and header length.
pub const APPLOADER_OFFSET: u64 = BOOT_LEN + BI2_LEN;
const APPLOADER_HEADER: u64 = 0x20;
const DOL_HEADER: u64 = 0x100;
/// A junk match shorter than this inside a residue run is chance, not
/// junk (the generator's bytes are uniform: 16 matching bytes by
/// accident is 2^-128).
const MIN_JUNK_RUN: usize = 16;
/// Bytes per read while classifying gaps.
const SCAN_BYTES: usize = 1 << 20;
/// A uniform tail at least this long splits off a residue run as a
/// fill (the NDS classifier's rule).
const MIN_PAD_SPLIT: usize = 512;
/// Entry cap across the FST (a retail disc has thousands).
const MAX_ENTRIES: u64 = 1 << 20;
/// An apploader, DOL or FST above this is a lie.
const MAX_SYS_LEN: u64 = 64 << 20;

/// Structural refusals — deterministic conclusions about the bytes.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("not a gamecube disc (no magic at 0x1c)")]
    NotGcm,
    #[error("a wii disc (magic at 0x18) — not a gamecube image")]
    Wii,
    #[error("truncated: {0}")]
    Truncated(&'static str),
    #[error("header: {0}")]
    Header(String),
    #[error("fst: {0}")]
    Fst(String),
    #[error("declared ranges overlap: {0} and {1}")]
    Overlap(String, String),
}

#[derive(Debug, thiserror::Error)]
pub enum GcmError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Refused(#[from] Refusal),
}

/// The junk stream claimed alongside the pieces (D115): a zero-input
/// recipe's output over the whole address space of the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Junk {
    pub id: [u8; 4],
    pub disc: u8,
    /// Stream length in bytes — the image length.
    pub len: u64,
    /// blake3 of the whole stream — the output identity of the recipe.
    pub hash: Blake3,
}

#[derive(Debug)]
pub struct Layout {
    pub total_len: u64,
    pub game_id: [u8; 6],
    pub disc: u8,
    /// In physical (coverage) order; regions reference these by index,
    /// and [`Region::Extern`] regions reference the junk stream (extern
    /// input 0) at the same offsets.
    pub pieces: Vec<Piece>,
    pub regions: Vec<Region>,
    /// `Some` when any junk matched.
    pub junk: Option<Junk>,
    pub file_count: usize,
    pub dir_count: usize,
    pub empty_files: usize,
    /// Bytes matched against the generator / uniform fill / residue.
    pub junk_bytes: u64,
    pub fill_bytes: u64,
    pub residual_bytes: u64,
    /// True when the entry count exceeded the piece cap and pieces are
    /// contiguous data runs instead of files.
    pub coalesced: bool,
}

/// One declared byte range.
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    start: u64,
    len: u64,
    literal: bool,
}

/// Big-endian u32 at `at` (GameCube is big-endian; the NDS helpers are
/// little-endian).
fn be32(buf: &[u8], at: usize) -> u64 {
    u64::from(u32::from_be_bytes([
        buf[at],
        buf[at + 1],
        buf[at + 2],
        buf[at + 3],
    ]))
}

/// True if `head` (≥ 0x20 bytes at offset 0) carries the GameCube magic.
#[must_use]
pub fn looks_like_gcm(head: &[u8]) -> bool {
    head.len() >= 0x20 && head[0x1C..0x20] == MAGIC
}

/// Parse the image into its coverage map. `max_pieces` caps the
/// per-entry decomposition — above it, pieces are contiguous data runs.
///
/// # Errors
/// [`GcmError::Refused`] is a settled conclusion; `Io` is environmental.
pub fn parse_layout<R: Read + Seek>(img: &mut R, max_pieces: usize) -> Result<Layout, GcmError> {
    let total_len = img.seek(SeekFrom::End(0))?;
    if total_len < APPLOADER_OFFSET + APPLOADER_HEADER {
        return Err(Refusal::NotGcm.into());
    }
    let boot = read_at(img, 0, usize::try_from(BOOT_LEN).expect("small"))?;
    if boot[0x18..0x1C] == WII_MAGIC {
        return Err(Refusal::Wii.into());
    }
    if !looks_like_gcm(&boot) {
        return Err(Refusal::NotGcm.into());
    }
    let id4: [u8; 4] = boot[..4].try_into().expect("4 bytes");
    parse_volume(
        img,
        max_pieces,
        VolumeOpts {
            junk_id: id4,
            junk_disc: boot[6],
            wii: false,
            hash_junk: true,
        },
    )
}

/// How a volume is walked (D116 shares this walker between a GameCube
/// image and a Wii partition's plaintext): the junk stream its gaps are
/// compared against — a GameCube disc's own ID, or for a Wii partition
/// the DISC header's ID over the partition's plaintext address space,
/// the mastering tool's rule — whether the boot block and FST carry
/// Wii's `>> 2` offsets, and whether the stream's identity is hashed
/// here (a Wii disc hashes ONE stream for all its partitions and gaps).
#[derive(Debug, Clone, Copy)]
pub struct VolumeOpts {
    pub junk_id: [u8; 4],
    pub junk_disc: u8,
    pub wii: bool,
    pub hash_junk: bool,
}

/// Parse a GameCube-shaped volume — a disc image, or a Wii partition's
/// plaintext — into its coverage map. The boot block's magic is the
/// caller's business ([`parse_layout`] checks GameCube's; a Wii
/// partition's boot block carries the Wii magic and is checked here
/// when `opts.wii`).
///
/// # Errors
/// [`GcmError::Refused`] is a settled conclusion; `Io` is environmental.
pub fn parse_volume<R: Read + Seek>(
    img: &mut R,
    max_pieces: usize,
    opts: VolumeOpts,
) -> Result<Layout, GcmError> {
    let total_len = img.seek(SeekFrom::End(0))?;
    if total_len < APPLOADER_OFFSET + APPLOADER_HEADER {
        return Err(Refusal::Truncated("no room for the boot block and apploader header").into());
    }
    let boot = read_at(img, 0, usize::try_from(BOOT_LEN).expect("small"))?;
    if opts.wii && boot[0x18..0x1C] != WII_MAGIC {
        return Err(Refusal::Header("partition boot block lacks the wii magic".into()).into());
    }
    let game_id: [u8; 6] = boot[..6].try_into().expect("6 bytes");
    let disc = boot[6];
    // Wii stores every offset (and the FST size) shifted right by two.
    let shift = if opts.wii { 2 } else { 0 };
    let dol_offset = be32(&boot, 0x420) << shift;
    let fst_offset = be32(&boot, 0x424) << shift;
    let fst_size = be32(&boot, 0x428) << shift;

    let mut entries: Vec<Entry> = vec![
        Entry {
            name: "sys/boot.bin".into(),
            start: 0,
            len: BOOT_LEN,
            literal: true,
        },
        Entry {
            name: "sys/bi2.bin".into(),
            start: BOOT_LEN,
            len: BI2_LEN,
            literal: false,
        },
    ];

    // Apploader: a 0x20 header carrying the code and trailer sizes.
    let ah = read_at(
        img,
        APPLOADER_OFFSET,
        usize::try_from(APPLOADER_HEADER).expect("small"),
    )?;
    let apploader_len = APPLOADER_HEADER + be32(&ah, 0x14) + be32(&ah, 0x18);
    if apploader_len > MAX_SYS_LEN {
        return Err(Refusal::Header(format!("apploader of {apploader_len} bytes")).into());
    }
    if APPLOADER_OFFSET + apploader_len > total_len {
        return Err(Refusal::Truncated("apploader past the image").into());
    }
    entries.push(Entry {
        name: "sys/apploader.img".into(),
        start: APPLOADER_OFFSET,
        len: apploader_len,
        literal: false,
    });

    // DOL: its size is the end of its furthest section.
    if dol_offset + DOL_HEADER > total_len {
        return Err(Refusal::Truncated("dol header past the image").into());
    }
    let dh = read_at(img, dol_offset, usize::try_from(DOL_HEADER).expect("small"))?;
    let mut dol_len = DOL_HEADER;
    for i in 0..18 {
        let off = be32(&dh, i * 4);
        let size = be32(&dh, 0x90 + i * 4);
        if size > 0 {
            dol_len = dol_len.max(off + size);
        }
    }
    if dol_len > MAX_SYS_LEN {
        return Err(Refusal::Header(format!("dol of {dol_len} bytes")).into());
    }
    if dol_offset + dol_len > total_len {
        return Err(Refusal::Truncated("dol past the image").into());
    }
    entries.push(Entry {
        name: "sys/main.dol".into(),
        start: dol_offset,
        len: dol_len,
        literal: false,
    });

    // FST: 12-byte nodes then a string table; the root's length is the
    // node count, a directory's offset is its parent and its length the
    // index past its last descendant.
    if !(12..=MAX_SYS_LEN).contains(&fst_size) {
        return Err(Refusal::Fst(format!("table of {fst_size} bytes")).into());
    }
    if fst_offset + fst_size > total_len {
        return Err(Refusal::Truncated("fst past the image").into());
    }
    entries.push(Entry {
        name: "sys/fst.bin".into(),
        start: fst_offset,
        len: fst_size,
        literal: false,
    });
    let fst = read_at(img, fst_offset, usize::try_from(fst_size).expect("capped"))?;
    let count = be32(&fst, 8);
    if count == 0 || count > MAX_ENTRIES || count * 12 > fst_size {
        return Err(Refusal::Fst(format!("{count} nodes in {fst_size} bytes")).into());
    }
    let strings = &fst[usize::try_from(count * 12).expect("bounded")..];
    let mut file_count = 0usize;
    let mut dir_count = 1usize;
    let mut empty_files = 0usize;
    let mut stack: Vec<(u64, String)> = vec![(count, String::new())];
    let mut seen_names: HashSet<String> = HashSet::new();
    for i in 1..count {
        while stack.last().is_some_and(|(end, _)| i >= *end) {
            stack.pop();
        }
        let node = &fst[usize::try_from(i * 12).expect("bounded")..][..12];
        let name_off =
            usize::from(node[1]) << 16 | usize::from(node[2]) << 8 | usize::from(node[3]);
        let name = strings
            .get(name_off..)
            .and_then(|s| s.split(|&b| b == 0).next())
            .ok_or_else(|| Refusal::Fst(format!("node {i} names past the string table")))?;
        let name = String::from_utf8_lossy(name);
        let parent = stack.last().map(|(_, p)| p.as_str()).unwrap_or("");
        let full = format!("{parent}/{name}");
        let offset = be32(node, 4);
        let length = be32(node, 8);
        match node[0] {
            1 => {
                if length <= i || length > count {
                    return Err(Refusal::Fst(format!("directory {full} ends at {length}")).into());
                }
                dir_count += 1;
                stack.push((length, full));
            }
            0 => {
                let offset = offset << shift;
                if length == 0 {
                    empty_files += 1;
                    continue;
                }
                if offset + length > total_len {
                    return Err(Refusal::Truncated("file extent past the image").into());
                }
                file_count += 1;
                // Two FST entries over one extent are one piece; the
                // name that sorts first wins (deterministic).
                let name = if seen_names.insert(full.clone()) {
                    full
                } else {
                    format!("{full}#{i}")
                };
                entries.push(Entry {
                    name,
                    start: offset,
                    len: length,
                    literal: false,
                });
            }
            k => return Err(Refusal::Fst(format!("node {i} has kind {k}")).into()),
        }
    }

    entries.sort_by_key(|e| (e.start, e.len));
    entries.dedup_by(|b, a| a.start == b.start && a.len == b.len);
    for pair in entries.windows(2) {
        if pair[1].start < pair[0].start + pair[0].len {
            return Err(Refusal::Overlap(pair[0].name.clone(), pair[1].name.clone()).into());
        }
    }

    // Above the piece cap, pieces are contiguous data runs (junk-sector
    // aligned) — recipe volume bounded, junk classification unchanged.
    let coalesced = entries.len() > max_pieces;
    let declared: Vec<Entry> = if coalesced {
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
                    len: (end - start).min(total_len - start),
                    literal: false,
                }),
            }
        }
        runs
    } else {
        entries
    };

    let mut cls = Classifier::new(img, opts.junk_id, opts.junk_disc);
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

    // The stream's identity: hash the whole address space once. Only
    // when something matched — a zero-padded master claims no junk.
    let junk = (opts.hash_junk && cls.junk_bytes > 0)
        .then(|| junk_stream(opts.junk_id, opts.junk_disc, total_len));
    let junk_bytes = cls.junk_bytes;
    let fill_bytes = cls.fill_bytes;
    let residual_bytes = cls.residual_bytes;

    Ok(Layout {
        total_len,
        game_id,
        disc,
        pieces,
        regions,
        junk,
        file_count,
        dir_count,
        empty_files,
        junk_bytes,
        fill_bytes,
        residual_bytes,
        coalesced,
    })
}

/// The junk stream's identity over `[0, len)`: generate and hash it
/// once. Seconds per GiB — the dominant cost of a walk (D115 watch item).
#[must_use]
pub fn junk_stream(id: [u8; 4], disc: u8, len: u64) -> Junk {
    let mut hasher = blake3::Hasher::new();
    let mut lfg = Lfg::default();
    let mut buf = vec![0u8; SCAN_BYTES];
    let mut pos = 0u64;
    while pos < len {
        let n = usize::try_from((len - pos).min(SCAN_BYTES as u64)).expect("bounded");
        fill_at(&mut lfg, id, disc, pos, &mut buf[..n]);
        hasher.update(&buf[..n]);
        pos += n as u64;
    }
    Junk {
        id,
        disc,
        len,
        hash: Blake3(*hasher.finalize().as_bytes()),
    }
}

/// A run of like-classified bytes, pending emission as one region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    /// Junk at absolute `start` — the stream at the same offsets.
    Junk {
        start: u64,
        len: u64,
    },
    Fill {
        byte: u8,
        start: u64,
        len: u64,
    },
    Residue {
        start: u64,
        len: u64,
    },
}

/// The gap classifier: reads the gaps between declared ranges
/// sequentially, comparing each against the generator at its own
/// position. Shared with the Wii walker (D116), whose disc-level gaps
/// classify against the same generator at disc offsets.
pub(crate) struct Classifier<'r, R> {
    img: &'r mut R,
    id: [u8; 4],
    disc: u8,
    lfg: Lfg,
    pending: Option<Run>,
    pub(crate) junk_bytes: u64,
    pub(crate) fill_bytes: u64,
    pub(crate) residual_bytes: u64,
    buf: Vec<u8>,
    junk_buf: Vec<u8>,
    eq_run: Vec<usize>,
    uni_run: Vec<usize>,
}

impl<'r, R: Read + Seek> Classifier<'r, R> {
    pub(crate) fn new(img: &'r mut R, id: [u8; 4], disc: u8) -> Self {
        Self {
            img,
            id,
            disc,
            lfg: Lfg::default(),
            pending: None,
            junk_bytes: 0,
            fill_bytes: 0,
            residual_bytes: 0,
            buf: vec![0u8; SCAN_BYTES],
            junk_buf: Vec::new(),
            eq_run: Vec::new(),
            uni_run: Vec::new(),
        }
    }

    pub(crate) fn classify(
        &mut self,
        start: u64,
        len: u64,
        regions: &mut Vec<Region>,
        pieces: &mut Vec<Piece>,
    ) -> Result<(), GcmError> {
        if len == 0 {
            return Ok(());
        }
        self.img.seek(SeekFrom::Start(start))?;
        let mut buf = std::mem::take(&mut self.buf);
        let end = start + len;
        let mut pos = start;
        while pos < end {
            // Read up to the buffer, cut at a sector boundary so every
            // chunk classified below lies within one reseed.
            let take = (end - pos).min(buf.len() as u64);
            let take = if take == end - pos {
                take
            } else {
                (pos + take) / SECTOR * SECTOR - pos
            };
            let n = usize::try_from(take.max(1).min(end - pos)).expect("bounded");
            self.img.read_exact(&mut buf[..n])?;
            let mut off = 0usize;
            while off < n {
                let abs = pos + off as u64;
                let room = usize::try_from(SECTOR - abs % SECTOR)
                    .expect("< sector")
                    .min(n - off);
                self.classify_chunk(abs, &buf[off..off + room], regions, pieces);
                off += room;
            }
            pos += n as u64;
        }
        self.buf = buf;
        self.flush(regions, pieces);
        Ok(())
    }

    /// Classify one chunk that lies within a single junk sector: the
    /// expected junk is generated once, then the chunk is walked as
    /// runs — junk (equal to the generator for at least
    /// [`MIN_JUNK_RUN`] bytes, or to the end), fill (one byte value for
    /// at least [`MIN_PAD_SPLIT`] bytes, or to the end), and residue
    /// up to wherever the next junk or fill run begins. Run lengths
    /// are precomputed backwards so the walk is linear.
    fn classify_chunk(
        &mut self,
        abs: u64,
        chunk: &[u8],
        regions: &mut Vec<Region>,
        pieces: &mut Vec<Piece>,
    ) {
        let n = chunk.len();
        let mut expect = std::mem::take(&mut self.junk_buf);
        expect.resize(n, 0);
        fill_at(&mut self.lfg, self.id, self.disc, abs, &mut expect[..n]);
        let mut eq_run = std::mem::take(&mut self.eq_run);
        let mut uni_run = std::mem::take(&mut self.uni_run);
        eq_run.clear();
        uni_run.clear();
        eq_run.resize(n + 1, 0);
        uni_run.resize(n + 1, 0);
        for i in (0..n).rev() {
            eq_run[i] = if chunk[i] == expect[i] {
                eq_run[i + 1] + 1
            } else {
                0
            };
            uni_run[i] = if i + 1 < n && chunk[i] == chunk[i + 1] {
                uni_run[i + 1] + 1
            } else {
                1
            };
        }
        let is_junk = |p: usize| eq_run[p] >= MIN_JUNK_RUN || eq_run[p] == n - p;
        let is_fill = |p: usize| uni_run[p] >= MIN_PAD_SPLIT || uni_run[p] == n - p;
        let mut p = 0usize;
        while p < n {
            let at = abs + p as u64;
            if is_junk(p) {
                let run = eq_run[p];
                self.push(
                    Run::Junk {
                        start: at,
                        len: run as u64,
                    },
                    regions,
                    pieces,
                );
                p += run;
            } else if is_fill(p) {
                let run = uni_run[p];
                self.push(
                    Run::Fill {
                        byte: chunk[p],
                        start: at,
                        len: run as u64,
                    },
                    regions,
                    pieces,
                );
                p += run;
            } else {
                let mut q = p + 1;
                while q < n && !is_junk(q) && !is_fill(q) {
                    q += 1;
                }
                self.push(
                    Run::Residue {
                        start: at,
                        len: (q - p) as u64,
                    },
                    regions,
                    pieces,
                );
                p = q;
            }
        }
        self.junk_buf = expect;
        self.eq_run = eq_run;
        self.uni_run = uni_run;
    }

    fn push(&mut self, next: Run, regions: &mut Vec<Region>, pieces: &mut Vec<Piece>) {
        match (self.pending.as_mut(), next) {
            (Some(Run::Junk { start, len }), Run::Junk { start: s, len: l })
                if *start + *len == s =>
            {
                *len += l;
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
            Run::Junk { start, len } => {
                self.junk_bytes += len;
                regions.push(Region::Extern {
                    input: 0,
                    offset: start,
                    len,
                });
            }
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

/// Synthetic image builder for the gates: a small GameCube image (not
/// 1.46 GB — the analyzer assumes nothing about size) with real junk
/// from the generator between and after the files, a zero pad, a
/// residue gap, an empty file, and a nested directory tree.
#[doc(hidden)]
pub mod synth {
    use super::{APPLOADER_OFFSET, BI2_LEN, BOOT_LEN, DOL_HEADER, MAGIC, SECTOR};
    use datboi_xf_gc_junk::{Lfg, fill_at};

    /// What the builder laid out, for assertions.
    pub struct Image {
        pub bytes: Vec<u8>,
        pub id: [u8; 6],
        pub disc: u8,
        /// `(path, bytes)` in FST order.
        pub files: Vec<(String, Vec<u8>)>,
        /// The residue gap's `(start, len)`.
        pub residue: (u64, u64),
        pub apploader: Vec<u8>,
        pub dol: Vec<u8>,
    }

    /// Kind of bytes the builder writes into unused space.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Pad {
        /// The mastering generator's junk.
        Junk,
        /// Zeros (an NKit-scrubbed or zero-padded master).
        Zero,
    }

    fn pattern(len: usize, salt: u32) -> Vec<u8> {
        (0..len)
            .map(|i| ((i as u32).wrapping_mul(2_654_435_761) ^ salt).to_le_bytes()[i % 4])
            .collect()
    }

    /// Build the image. `files` are `(path, bytes)`; directories are
    /// created from the paths. Total size: a few MiB.
    #[must_use]
    pub fn image(id: [u8; 6], disc: u8, files: &[(&str, Vec<u8>)], pad: Pad) -> Image {
        volume(id, disc, files, pad, false)
    }

    /// [`image`], or with `wii` the plaintext of a Wii partition: the
    /// Wii magic at 0x18, the boot block's offsets and the FST's file
    /// offsets stored `>> 2`, junk seeded exactly as a GameCube disc's
    /// would be (the D116 walker's rule: the disc ID over the
    /// partition's own address space).
    #[must_use]
    pub fn volume(id: [u8; 6], disc: u8, files: &[(&str, Vec<u8>)], pad: Pad, wii: bool) -> Image {
        let shift = if wii { 2 } else { 0 };
        let apploader = {
            let body = pattern(5000, 0xA11);
            let mut a = vec![0u8; 0x20];
            a[..10].copy_from_slice(b"2001/12/17");
            a[0x10..0x14].copy_from_slice(&0x8120_0000u32.to_be_bytes());
            a[0x14..0x18].copy_from_slice(&(body.len() as u32).to_be_bytes());
            a[0x18..0x1C].copy_from_slice(&0u32.to_be_bytes());
            a.extend_from_slice(&body);
            a
        };
        let dol = {
            let text = pattern(3000, 0xD01);
            let mut d = vec![0u8; usize::try_from(DOL_HEADER).unwrap()];
            d[..4].copy_from_slice(&0x100u32.to_be_bytes()); // text 0 offset
            d[0x90..0x94].copy_from_slice(&(text.len() as u32).to_be_bytes());
            d[0x48..0x4C].copy_from_slice(&0x8000_3100u32.to_be_bytes());
            d[0xE0..0xE4].copy_from_slice(&0x8000_3100u32.to_be_bytes());
            d.extend_from_slice(&text);
            d
        };

        // FST: root, then directories/files in path order (one level
        // of nesting is enough for the gates).
        struct Node {
            dir: bool,
            name: String,
            offset: u32,
            length: u32,
        }
        let mut nodes: Vec<Node> = vec![Node {
            dir: true,
            name: String::new(),
            offset: 0,
            length: 0,
        }];
        // Layout files after the FST; FST after the DOL.
        let apploader_end = APPLOADER_OFFSET + apploader.len() as u64;
        let dol_offset = apploader_end.div_ceil(SECTOR) * SECTOR;
        let fst_offset = (dol_offset + dol.len() as u64 + 0x1234).div_ceil(32) * 32;
        // Provisional FST size: 12 bytes per node + names.
        let mut placed: Vec<(usize, u64)> = Vec::new(); // (file index, offset)
        let mut dirs: Vec<(String, usize)> = Vec::new(); // (dir name, node index)
        let fst_size_guess = 12 * (files.len() + 8) as u64
            + files.iter().map(|(p, _)| p.len() as u64 + 1).sum::<u64>()
            + 64;
        let mut next = (fst_offset + fst_size_guess).div_ceil(SECTOR) * SECTOR + 0x500;
        for (ix, (path, bytes)) in files.iter().enumerate() {
            let (dir, name) = match path.trim_start_matches('/').split_once('/') {
                Some((d, n)) => (Some(d.to_owned()), n.to_owned()),
                None => (None, path.trim_start_matches('/').to_owned()),
            };
            if let Some(d) = &dir
                && !dirs.iter().any(|(n, _)| n == d)
            {
                dirs.push((d.clone(), nodes.len()));
                nodes.push(Node {
                    dir: true,
                    name: d.clone(),
                    offset: 0,
                    length: 0,
                });
            }
            let offset = if bytes.is_empty() { 0 } else { next };
            placed.push((ix, offset));
            nodes.push(Node {
                dir: false,
                name,
                offset: u32::try_from(offset >> shift).unwrap(),
                length: u32::try_from(bytes.len()).unwrap(),
            });
            if let Some(d) = &dir {
                let di = dirs.iter().find(|(n, _)| n == d).unwrap().1;
                nodes[di].length = nodes.len() as u32;
            }
            if !bytes.is_empty() {
                // Files are 4-byte aligned with a junk-sized gap now
                // and then, sector aligned every other file.
                next += bytes.len() as u64;
                next = if ix % 2 == 0 {
                    next.div_ceil(SECTOR) * SECTOR
                } else {
                    next.div_ceil(4) * 4 + 0x120
                };
            }
        }
        nodes[0].length = nodes.len() as u32;
        let mut fst = Vec::new();
        let mut strings = Vec::new();
        for n in &nodes {
            fst.push(u8::from(n.dir));
            let so = strings.len() as u32;
            fst.extend_from_slice(&so.to_be_bytes()[1..]);
            fst.extend_from_slice(&n.offset.to_be_bytes());
            fst.extend_from_slice(&n.length.to_be_bytes());
            strings.extend_from_slice(n.name.as_bytes());
            strings.push(0);
        }
        fst.extend_from_slice(&strings);
        // Wii stores the FST size `>> 2`: keep it a multiple of four
        // (a real master's string table is padded the same way).
        while fst.len() % 4 != 0 {
            fst.push(0);
        }
        assert!(fst.len() as u64 <= fst_size_guess, "fst guess holds");

        // A residue gap and a zero pad after the last file, then the end.
        let residue_start = next.div_ceil(SECTOR) * SECTOR + 0x40;
        let residue_len = 6000u64;
        let zero_start = residue_start + residue_len;
        let zero_len = 3 * SECTOR + 17;
        let total = (zero_start + zero_len + 5 * SECTOR).div_ceil(SECTOR) * SECTOR;

        let mut out = vec![0u8; usize::try_from(total).unwrap()];
        match pad {
            Pad::Junk => {
                let mut lfg = Lfg::default();
                let id4: [u8; 4] = id[..4].try_into().unwrap();
                fill_at(&mut lfg, id4, disc, 0, &mut out);
            }
            Pad::Zero => {}
        }
        // Header.
        out[..6].copy_from_slice(&id);
        out[6] = disc;
        out[7] = 0;
        out[8..0x20].fill(0);
        if wii {
            out[0x18..0x1C].copy_from_slice(&super::WII_MAGIC);
        } else {
            out[0x1C..0x20].copy_from_slice(&MAGIC);
        }
        out[0x20..0x60].fill(0);
        out[0x20..0x2B].copy_from_slice(b"DATBOI TEST");
        out[0x60..0x420].fill(0);
        out[0x420..0x424]
            .copy_from_slice(&u32::try_from(dol_offset >> shift).unwrap().to_be_bytes());
        out[0x424..0x428]
            .copy_from_slice(&u32::try_from(fst_offset >> shift).unwrap().to_be_bytes());
        out[0x428..0x42C]
            .copy_from_slice(&u32::try_from(fst.len() >> shift).unwrap().to_be_bytes());
        out[0x42C..0x430]
            .copy_from_slice(&u32::try_from(fst.len() >> shift).unwrap().to_be_bytes());
        out[0x430..0x440].fill(0);
        let bi2 = pattern(usize::try_from(BI2_LEN).unwrap(), 0xB12);
        out[usize::try_from(BOOT_LEN).unwrap()..usize::try_from(APPLOADER_OFFSET).unwrap()]
            .copy_from_slice(&bi2);
        let at = usize::try_from(APPLOADER_OFFSET).unwrap();
        out[at..at + apploader.len()].copy_from_slice(&apploader);
        let at = usize::try_from(dol_offset).unwrap();
        out[at..at + dol.len()].copy_from_slice(&dol);
        let at = usize::try_from(fst_offset).unwrap();
        out[at..at + fst.len()].copy_from_slice(&fst);
        let mut laid: Vec<(String, Vec<u8>)> = Vec::new();
        for (ix, offset) in &placed {
            let (path, bytes) = &files[*ix];
            let at = usize::try_from(*offset).unwrap();
            out[at..at + bytes.len()].copy_from_slice(bytes);
            laid.push((path.to_string(), bytes.clone()));
        }
        let at = usize::try_from(residue_start).unwrap();
        let res = pattern(usize::try_from(residue_len).unwrap(), 0x5E5);
        out[at..at + res.len()].copy_from_slice(&res);
        let at = usize::try_from(zero_start).unwrap();
        out[at..at + usize::try_from(zero_len).unwrap()].fill(0);
        Image {
            bytes: out,
            id,
            disc,
            files: laid,
            residue: (residue_start, residue_len),
            apploader,
            dol,
        }
    }
}
