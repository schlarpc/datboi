//! FAT32 filesystem-layout math (D62) — pure, deterministic, no I/O.
//!
//! Turns a path-sorted file list into the exact byte layout of a FAT32
//! disk image, expressed as symbolic segments: small literal sectors
//! (MBR, boot, FSInfo), windows into two synthesized skeleton buffers
//! (the FAT and the packed directory clusters), whole-file windows, and
//! zero fill. The caller ([`crate::image`]) maps these onto
//! `assemble@1` segments; nothing here touches a store or a clock.
//!
//! Identity is pinned by construction (D62): fixed timestamps
//! (2000-01-01 00:00:00), volume serial and disk signature supplied by
//! the caller (derived from the snapshot hash), strictly sequential
//! cluster chains, and allocation in manifest order. Same inputs, same
//! bytes — the skeleton goldens below are format commitments.
//!
//! Correctness here is a MINTING property no runtime verification can
//! catch (a wrong FAT chain serves faithfully-wrong bytes), which is
//! why the fsck-in-CI gate exists at the same rank as these tests.

use datboi_core::hash::Blake3;

/// Bytes per sector; FAT32 supports others, every target device uses 512.
pub const SECTOR: u64 = 512;
/// Reserved sectors before the first FAT (mkfs.fat's FAT32 convention).
const RESERVED_SECTORS: u64 = 32;
/// Partition start in sectors (1 MiB alignment, the modern convention).
const PARTITION_LBA: u64 = 2048;
/// FAT32's legal minimum data-cluster count; fewer means the volume
/// would be interpreted as FAT16. Short layouts pad with free clusters.
const MIN_CLUSTERS: u64 = 65_525;
/// Largest legal FAT32 data-cluster count (cluster ids stop at
/// 0x0FFF_FFF4; 0x0FFF_FFF5.. are reserved/EOC values).
const MAX_CLUSTERS: u64 = 0x0FFF_FFF4 - 2;
/// End-of-chain marker.
const EOC: u32 = 0x0FFF_FFFF;
/// 2000-01-01 in FAT date encoding (years since 1980 ≪ 9 | month ≪ 5 | day).
const FAT_DATE: u16 = 0x2821;
/// 00:00:00 in FAT time encoding.
const FAT_TIME: u16 = 0x0000;
/// A directory may not exceed 65,536 32-byte entry slots (2 MiB).
const MAX_DIR_ENTRY_SLOTS: u64 = 65_536;
/// FAT32 directory entries hold a u32 size: 4 GiB − 1 hard ceiling.
const MAX_FILE_SIZE: u64 = (4 << 30) - 1;

/// One file from the view manifest, in row (path-sorted) order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Canonical viewsnap path: relative, `/`-separated.
    pub path: String,
    pub size: u64,
}

/// Identity-pinning image parameters (D62).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fat32Params {
    /// 11 bytes, space-padded, already label-legal (see [`label_for`]).
    pub volume_label: [u8; 11],
    /// Volume serial (snapshot hash bytes 0..4, LE).
    pub serial: u32,
    /// MBR disk signature (snapshot hash bytes 4..8, LE).
    pub disk_signature: u32,
    /// Bytes per cluster: power of two in 512..=65536.
    pub cluster_size: u32,
    /// Emit an MBR partition table (default for SD cards); `false`
    /// yields a superfloppy (bare filesystem at offset 0).
    pub partition: bool,
}

/// A run of output bytes, in output order. Offsets in `Fat` / `Dirs`
/// index into [`Fat32Layout::fat`] / [`Fat32Layout::dirs`]; `File`
/// references the input list by index and always starts cluster-aligned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutSegment {
    /// Literal sector bytes (MBR, boot sector, FSInfo; each ≤ 512).
    Literal(Vec<u8>),
    /// Window into the FAT skeleton buffer (emitted twice: two copies).
    Fat { offset: u64, len: u64 },
    /// Window into the packed directory-clusters skeleton buffer.
    Dirs { offset: u64, len: u64 },
    /// The whole content of `files[file_ix]`.
    File { file_ix: usize, len: u64 },
    /// Zero fill: alignment gaps, cluster slack, free-cluster padding.
    Fill { len: u64 },
}

/// Where everything landed; enough for tests, fsck slicing, and the
/// mint's segment mapping to reason about the image without re-deriving
/// the math.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Absolute byte offset of the filesystem (0 or 1 MiB).
    pub partition_offset: u64,
    /// Absolute byte offset of FAT copy 1.
    pub fat_offset: u64,
    /// Bytes in one FAT copy (sector-padded).
    pub fat_bytes: u64,
    /// Absolute byte offset of cluster 2.
    pub data_offset: u64,
    pub cluster_size: u32,
    /// Total data clusters, including free padding up to the FAT32 minimum.
    pub n_clusters: u64,
    /// Clusters allocated to directories and file content.
    pub used_clusters: u64,
    /// Clusters holding directory entries (a prefix of the used run).
    pub dir_clusters: u64,
    pub total_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fat32Layout {
    pub geometry: Geometry,
    /// One FAT copy (the image contains two identical windows over it).
    pub fat: Vec<u8>,
    /// Every directory cluster, packed in allocation order (root first).
    pub dirs: Vec<u8>,
    pub segments: Vec<LayoutSegment>,
}

#[derive(Debug, thiserror::Error)]
pub enum Fat32Error {
    #[error("cluster size {0} invalid: power of two in 512..=65536 required")]
    BadClusterSize(u32),
    #[error("path not FAT-legal: {0}")]
    BadPath(String),
    #[error("path component exceeds 255 UTF-16 units: {0}")]
    NameTooLong(String),
    #[error("file too large for FAT32 (max 4 GiB − 1): {path} is {size} bytes")]
    FileTooLarge { path: String, size: u64 },
    #[error("conflicting paths (FAT is case-insensitive, dirs and files share a namespace): {0}")]
    PathConflict(String),
    #[error("input not strictly path-sorted at: {0}")]
    Unsorted(String),
    #[error("directory {0} exceeds FAT32's 65,536-entry limit")]
    DirTooLarge(String),
    #[error("layout needs {0} data clusters, over FAT32's maximum")]
    TooManyClusters(u64),
    #[error("image of {0} bytes exceeds FAT32's 32-bit sector count")]
    ImageTooLarge(u64),
}

/// Derive a volume label from a view name: label-legal charset,
/// uppercased, truncated to 11 bytes, space-padded. Never blank.
#[must_use]
pub fn label_for(name: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    let mut i = 0;
    for c in name.chars() {
        if i == 11 {
            break;
        }
        let b = match c {
            'a'..='z' => c.to_ascii_uppercase() as u8,
            'A'..='Z' | '0'..='9' | '-' | '_' | ' ' => c as u8,
            _ => b'_',
        };
        out[i] = b;
        i += 1;
    }
    if i == 0 {
        out[..6].copy_from_slice(b"DATBOI");
    }
    out
}

impl Fat32Params {
    fn validate(&self) -> Result<(), Fat32Error> {
        let cs = self.cluster_size;
        if !cs.is_power_of_two() || !(512..=65_536).contains(&cs) {
            return Err(Fat32Error::BadClusterSize(cs));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Directory tree

#[derive(Debug, Clone, Copy)]
enum ChildKind {
    Dir(usize),
    File(usize),
}

#[derive(Debug)]
struct Child {
    name: String,
    kind: ChildKind,
    short: [u8; 11],
    needs_lfn: bool,
}

#[derive(Debug)]
struct Dir {
    /// `""` for root.
    path: String,
    parent: usize,
    children: Vec<Child>,
    first_cluster: u32,
    n_clusters: u64,
}

struct Tree {
    /// Index 0 is root; the rest in lexicographic path order.
    dirs: Vec<Dir>,
}

fn component_is_fat_legal(c: &str) -> bool {
    !c.is_empty()
        && c != "."
        && c != ".."
        && !c.ends_with('.')
        && !c.ends_with(' ')
        && !c.chars().any(|ch| {
            matches!(ch, '"' | '*' | '/' | ':' | '<' | '>' | '?' | '\\' | '|') || (ch as u32) < 0x20
        })
}

fn build_tree(files: &[FileEntry]) -> Result<Tree, Fat32Error> {
    use std::collections::BTreeMap;

    // Strictly ascending paths: the viewsnap invariant, re-checked here
    // because the layout's determinism depends on it.
    for w in files.windows(2) {
        if w[0].path >= w[1].path {
            return Err(Fat32Error::Unsorted(w[1].path.clone()));
        }
    }

    let mut dirs = vec![Dir {
        path: String::new(),
        parent: 0,
        children: Vec::new(),
        first_cluster: 0,
        n_clusters: 0,
    }];
    let mut by_path: BTreeMap<String, usize> = BTreeMap::new();
    by_path.insert(String::new(), 0);
    // Case-folded (dir, name) namespace: FAT is case-insensitive and
    // dirs/files share it.
    let mut names: std::collections::HashSet<(usize, String)> = std::collections::HashSet::new();

    let ensure_dir = |dirs: &mut Vec<Dir>,
                      by_path: &mut BTreeMap<String, usize>,
                      names: &mut std::collections::HashSet<(usize, String)>,
                      path: &str|
     -> Result<usize, Fat32Error> {
        if let Some(&ix) = by_path.get(path) {
            return Ok(ix);
        }
        // Create parents left to right.
        let mut parent = 0usize;
        let mut so_far = String::new();
        for comp in path.split('/') {
            if !so_far.is_empty() {
                so_far.push('/');
            }
            so_far.push_str(comp);
            if let Some(&ix) = by_path.get(&so_far) {
                parent = ix;
                continue;
            }
            if !component_is_fat_legal(comp) {
                return Err(Fat32Error::BadPath(so_far.clone()));
            }
            if comp.encode_utf16().count() > 255 {
                return Err(Fat32Error::NameTooLong(so_far.clone()));
            }
            let folded = comp.to_uppercase();
            if !names.insert((parent, folded)) {
                return Err(Fat32Error::PathConflict(so_far.clone()));
            }
            let ix = dirs.len();
            dirs.push(Dir {
                path: so_far.clone(),
                parent,
                children: Vec::new(),
                first_cluster: 0,
                n_clusters: 0,
            });
            dirs[parent].children.push(Child {
                name: comp.to_owned(),
                kind: ChildKind::Dir(ix),
                short: [0; 11],
                needs_lfn: false,
            });
            by_path.insert(so_far.clone(), ix);
            parent = ix;
        }
        Ok(parent)
    };

    for (file_ix, f) in files.iter().enumerate() {
        if f.size > MAX_FILE_SIZE {
            return Err(Fat32Error::FileTooLarge {
                path: f.path.clone(),
                size: f.size,
            });
        }
        let (dir_path, name) = match f.path.rsplit_once('/') {
            Some((d, n)) => (d, n),
            None => ("", f.path.as_str()),
        };
        if !component_is_fat_legal(name) {
            return Err(Fat32Error::BadPath(f.path.clone()));
        }
        if name.encode_utf16().count() > 255 {
            return Err(Fat32Error::NameTooLong(f.path.clone()));
        }
        let parent = ensure_dir(&mut dirs, &mut by_path, &mut names, dir_path)?;
        let folded = name.to_uppercase();
        if !names.insert((parent, folded)) {
            return Err(Fat32Error::PathConflict(f.path.clone()));
        }
        dirs[parent].children.push(Child {
            name: name.to_owned(),
            kind: ChildKind::File(file_ix),
            short: [0; 11],
            needs_lfn: false,
        });
    }

    // Entry order within a directory: plain byte sort of names —
    // consistent with the manifest's path sort, deterministic.
    for d in &mut dirs {
        d.children.sort_by(|a, b| a.name.cmp(&b.name));
    }
    // `dirs` itself is already in lexicographic path order: parents are
    // created before children and files arrive path-sorted.
    Ok(Tree { dirs })
}

// ---------------------------------------------------------------------
// Short names + LFN

/// 8.3 charset mangle for one char of a stem or extension.
fn mangle_char(c: char, lossy: &mut bool) -> Option<u8> {
    match c {
        'A'..='Z' | '0'..='9' => Some(c as u8),
        'a'..='z' => {
            // Case change is not "lossy" (no ~N needed), just non-identity.
            Some(c.to_ascii_uppercase() as u8)
        }
        '!' | '#' | '$' | '%' | '&' | '\'' | '(' | ')' | '-' | '@' | '^' | '_' | '`' | '{'
        | '}' | '~' => Some(c as u8),
        ' ' | '.' => {
            *lossy = true;
            None
        }
        _ => {
            *lossy = true;
            Some(b'_')
        }
    }
}

/// Deterministic 8.3 short-name generation: uppercase mangle, then a
/// `~N` numeric tail on loss or collision, `N` assigned in child order.
fn short_name(long: &str, used: &mut std::collections::HashSet<[u8; 11]>) -> ([u8; 11], bool) {
    let mut lossy = false;
    let (stem_src, ext_src) = match long.rsplit_once('.') {
        Some((s, e)) if !s.trim_start_matches('.').is_empty() => (s, Some(e)),
        _ => (long, None),
    };
    if stem_src.len() != stem_src.trim_start_matches('.').len() {
        lossy = true;
    }
    let stem_src = stem_src.trim_start_matches('.');

    let mut stem: Vec<u8> = Vec::with_capacity(8);
    for c in stem_src.chars() {
        if stem.len() == 8 {
            lossy = true;
            break;
        }
        if let Some(b) = mangle_char(c, &mut lossy) {
            stem.push(b);
        }
    }
    if stem.is_empty() {
        stem.push(b'_');
        lossy = true;
    }
    let mut ext: Vec<u8> = Vec::with_capacity(3);
    if let Some(e) = ext_src {
        for c in e.chars() {
            if ext.len() == 3 {
                lossy = true;
                break;
            }
            if let Some(b) = mangle_char(c, &mut lossy) {
                ext.push(b);
            }
        }
    }

    let render = |stem: &[u8], ext: &[u8]| -> [u8; 11] {
        let mut n = [b' '; 11];
        n[..stem.len()].copy_from_slice(stem);
        n[8..8 + ext.len()].copy_from_slice(ext);
        n
    };

    if !lossy {
        let candidate = render(&stem, &ext);
        if used.insert(candidate) {
            // Does the 8.3 rendering reproduce the long name exactly?
            let mut back = String::from_utf8_lossy(&stem).into_owned();
            if !ext.is_empty() {
                back.push('.');
                back.push_str(&String::from_utf8_lossy(&ext));
            }
            return (candidate, back != long);
        }
    }
    for n in 1u32.. {
        let tail = format!("~{n}");
        let keep = 8 - tail.len().min(8);
        let mut base = stem.clone();
        base.truncate(keep);
        base.extend_from_slice(tail.as_bytes());
        let candidate = render(&base, &ext);
        if used.insert(candidate) {
            return (candidate, true);
        }
    }
    unreachable!("u32 tail space exhausted");
}

/// The FAT LFN checksum of an 11-byte short name.
fn sfn_checksum(short: &[u8; 11]) -> u8 {
    short.iter().fold(0u8, |sum, &b| {
        ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(b)
    })
}

fn lfn_entry_count(name: &str) -> u64 {
    let units = name.encode_utf16().count() as u64;
    units.div_ceil(13)
}

/// Append the LFN entries (reverse-sequence order) then leave the SFN
/// slot to the caller.
fn push_lfn(out: &mut Vec<u8>, name: &str, checksum: u8) {
    let units: Vec<u16> = name.encode_utf16().collect();
    let n = units.len().div_ceil(13);
    for seq in (1..=n).rev() {
        let mut e = [0u8; 32];
        e[0] = u8::try_from(seq).expect("≤20 entries") | if seq == n { 0x40 } else { 0 };
        e[11] = 0x0F;
        e[13] = checksum;
        // Char slots: bytes 1..11 (5), 14..26 (6), 28..32 (2).
        const SLOTS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (i, &pos) in SLOTS.iter().enumerate() {
            let ix = (seq - 1) * 13 + i;
            let unit = match ix.cmp(&units.len()) {
                std::cmp::Ordering::Less => units[ix],
                std::cmp::Ordering::Equal => 0x0000,
                std::cmp::Ordering::Greater => 0xFFFF,
            };
            e[pos..pos + 2].copy_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&e);
    }
}

fn push_sfn(out: &mut Vec<u8>, short: &[u8; 11], attr: u8, first_cluster: u32, size: u32) {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(short);
    e[11] = attr;
    e[14..16].copy_from_slice(&FAT_TIME.to_le_bytes());
    e[16..18].copy_from_slice(&FAT_DATE.to_le_bytes());
    e[18..20].copy_from_slice(&FAT_DATE.to_le_bytes());
    e[20..22].copy_from_slice(
        &u16::try_from(first_cluster >> 16)
            .expect("u16")
            .to_le_bytes(),
    );
    e[22..24].copy_from_slice(&FAT_TIME.to_le_bytes());
    e[24..26].copy_from_slice(&FAT_DATE.to_le_bytes());
    e[26..28].copy_from_slice(
        &u16::try_from(first_cluster & 0xFFFF)
            .expect("u16")
            .to_le_bytes(),
    );
    e[28..32].copy_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&e);
}

const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_VOLUME_ID: u8 = 0x08;

// ---------------------------------------------------------------------
// Layout

/// Compute the full image layout. `files` must be strictly path-sorted
/// (the viewsnap row invariant).
pub fn layout(files: &[FileEntry], params: &Fat32Params) -> Result<Fat32Layout, Fat32Error> {
    params.validate()?;
    let cs = u64::from(params.cluster_size);
    let spc = cs / SECTOR; // sectors per cluster

    let mut tree = build_tree(files)?;

    // Short names + per-directory entry-slot counts.
    let mut dir_slots: Vec<u64> = Vec::with_capacity(tree.dirs.len());
    for (dix, d) in tree.dirs.iter_mut().enumerate() {
        let mut used = std::collections::HashSet::new();
        let mut slots: u64 = if dix == 0 {
            u64::from(params.volume_label != [b' '; 11])
        } else {
            2 // `.` and `..`
        };
        for c in &mut d.children {
            let (short, needs_lfn) = short_name(&c.name, &mut used);
            c.short = short;
            c.needs_lfn = needs_lfn;
            slots += 1 + if needs_lfn {
                lfn_entry_count(&c.name)
            } else {
                0
            };
        }
        if slots > MAX_DIR_ENTRY_SLOTS {
            return Err(Fat32Error::DirTooLarge(if d.path.is_empty() {
                "/".to_owned()
            } else {
                d.path.clone()
            }));
        }
        dir_slots.push(slots);
    }

    // Cluster allocation: directories first (root = cluster 2, then
    // lexicographic path order), then files in row order. Every chain
    // is a contiguous run.
    let mut cur: u64 = 2;
    for (dix, d) in tree.dirs.iter_mut().enumerate() {
        let bytes = dir_slots[dix] * 32;
        let n = bytes.div_ceil(cs).max(1);
        d.first_cluster = u32::try_from(cur).map_err(|_| Fat32Error::TooManyClusters(cur))?;
        d.n_clusters = n;
        cur += n;
    }
    let dir_clusters = cur - 2;
    let mut file_clusters: Vec<(u32, u64)> = Vec::with_capacity(files.len());
    for f in files {
        let n = f.size.div_ceil(cs);
        let first = if n == 0 {
            0
        } else {
            u32::try_from(cur).map_err(|_| Fat32Error::TooManyClusters(cur))?
        };
        file_clusters.push((first, n));
        cur += n;
    }
    let used_clusters = cur - 2;
    let n_clusters = used_clusters.max(MIN_CLUSTERS);
    if n_clusters > MAX_CLUSTERS {
        return Err(Fat32Error::TooManyClusters(n_clusters));
    }

    // FAT skeleton: one copy, sector-padded.
    let fat_sectors = ((n_clusters + 2) * 4).div_ceil(SECTOR);
    let fat_bytes = fat_sectors * SECTOR;
    let mut fat =
        vec![0u8; usize::try_from(fat_bytes).map_err(|_| Fat32Error::ImageTooLarge(fat_bytes))?];
    let set = |fat: &mut [u8], ix: u64, val: u32| {
        let o = usize::try_from(ix * 4).expect("fat index fits");
        fat[o..o + 4].copy_from_slice(&val.to_le_bytes());
    };
    set(&mut fat, 0, 0x0FFF_FF00 | 0xF8);
    set(&mut fat, 1, EOC);
    let chain = |fat: &mut [u8], first: u32, n: u64| {
        for i in 0..n {
            let c = u64::from(first) + i;
            let next = if i + 1 == n {
                EOC
            } else {
                u32::try_from(c + 1).expect("checked")
            };
            set(fat, c, next);
        }
    };
    for d in &tree.dirs {
        chain(&mut fat, d.first_cluster, d.n_clusters);
    }
    for &(first, n) in &file_clusters {
        if n > 0 {
            chain(&mut fat, first, n);
        }
    }

    // Directory clusters skeleton, packed in allocation order.
    let dirs_len = dir_clusters * cs;
    let mut dirs_buf = Vec::with_capacity(
        usize::try_from(dirs_len).map_err(|_| Fat32Error::ImageTooLarge(dirs_len))?,
    );
    for (dix, d) in tree.dirs.iter().enumerate() {
        let start = dirs_buf.len();
        if dix == 0 {
            if params.volume_label != [b' '; 11] {
                push_sfn(&mut dirs_buf, &params.volume_label, ATTR_VOLUME_ID, 0, 0);
            }
        } else {
            let parent_cluster = if d.parent == 0 {
                0
            } else {
                tree.dirs[d.parent].first_cluster
            };
            push_sfn(
                &mut dirs_buf,
                b".          ",
                ATTR_DIRECTORY,
                d.first_cluster,
                0,
            );
            push_sfn(
                &mut dirs_buf,
                b"..         ",
                ATTR_DIRECTORY,
                parent_cluster,
                0,
            );
        }
        for c in &d.children {
            if c.needs_lfn {
                push_lfn(&mut dirs_buf, &c.name, sfn_checksum(&c.short));
            }
            match c.kind {
                ChildKind::Dir(ix) => {
                    push_sfn(
                        &mut dirs_buf,
                        &c.short,
                        ATTR_DIRECTORY,
                        tree.dirs[ix].first_cluster,
                        0,
                    );
                }
                ChildKind::File(ix) => {
                    let (first, _) = file_clusters[ix];
                    let size = u32::try_from(files[ix].size).expect("checked ≤ 4 GiB − 1");
                    push_sfn(&mut dirs_buf, &c.short, ATTR_ARCHIVE, first, size);
                }
            }
        }
        let want = start + usize::try_from(d.n_clusters * cs).expect("dir cluster run fits");
        dirs_buf.resize(want, 0);
    }
    debug_assert_eq!(dirs_buf.len() as u64, dirs_len);

    // Geometry.
    let partition_offset = if params.partition {
        PARTITION_LBA * SECTOR
    } else {
        0
    };
    let partition_sectors = RESERVED_SECTORS + 2 * fat_sectors + n_clusters * spc;
    if u32::try_from(partition_sectors).is_err() {
        return Err(Fat32Error::ImageTooLarge(partition_sectors * SECTOR));
    }
    let total_size = partition_offset + partition_sectors * SECTOR;
    let geometry = Geometry {
        partition_offset,
        fat_offset: partition_offset + RESERVED_SECTORS * SECTOR,
        fat_bytes,
        data_offset: partition_offset + (RESERVED_SECTORS + 2 * fat_sectors) * SECTOR,
        cluster_size: params.cluster_size,
        n_clusters,
        used_clusters,
        dir_clusters,
        total_size,
    };

    // Literal sectors.
    let boot = boot_sector(params, partition_sectors, fat_sectors, spc);
    let fsinfo = fsinfo_sector(
        n_clusters - used_clusters,
        if n_clusters > used_clusters {
            u32::try_from(2 + used_clusters).expect("cluster ids checked")
        } else {
            0xFFFF_FFFF
        },
    );

    // Segments, in output order.
    let mut segments = Vec::new();
    if params.partition {
        segments.push(LayoutSegment::Literal(mbr_sector(
            params,
            partition_sectors,
        )));
        segments.push(LayoutSegment::Fill {
            len: partition_offset - SECTOR,
        });
    }
    segments.push(LayoutSegment::Literal(boot.clone()));
    segments.push(LayoutSegment::Literal(fsinfo.clone()));
    segments.push(LayoutSegment::Fill { len: 4 * SECTOR }); // sectors 2..6
    segments.push(LayoutSegment::Literal(boot)); // backup boot, sector 6
    segments.push(LayoutSegment::Literal(fsinfo)); // backup FSInfo, sector 7
    segments.push(LayoutSegment::Fill {
        len: (RESERVED_SECTORS - 8) * SECTOR,
    });
    segments.push(LayoutSegment::Fat {
        offset: 0,
        len: fat_bytes,
    });
    segments.push(LayoutSegment::Fat {
        offset: 0,
        len: fat_bytes,
    });
    if dirs_len > 0 {
        segments.push(LayoutSegment::Dirs {
            offset: 0,
            len: dirs_len,
        });
    }
    for (ix, f) in files.iter().enumerate() {
        let (_, n) = file_clusters[ix];
        if f.size > 0 {
            segments.push(LayoutSegment::File {
                file_ix: ix,
                len: f.size,
            });
        }
        let slack = n * cs - f.size;
        if slack > 0 {
            segments.push(LayoutSegment::Fill { len: slack });
        }
    }
    let free = (n_clusters - used_clusters) * cs;
    if free > 0 {
        segments.push(LayoutSegment::Fill { len: free });
    }
    debug_assert_eq!(
        segments.iter().map(segment_len).sum::<u64>(),
        total_size,
        "segment lengths must sum to the image size"
    );

    Ok(Fat32Layout {
        geometry,
        fat,
        dirs: dirs_buf,
        segments,
    })
}

/// Output length of one segment.
#[must_use]
pub fn segment_len(s: &LayoutSegment) -> u64 {
    match s {
        LayoutSegment::Literal(b) => b.len() as u64,
        LayoutSegment::Fat { len, .. }
        | LayoutSegment::Dirs { len, .. }
        | LayoutSegment::File { len, .. }
        | LayoutSegment::Fill { len } => *len,
    }
}

fn boot_sector(params: &Fat32Params, total_sectors: u64, fat_sectors: u64, spc: u64) -> Vec<u8> {
    let mut b = vec![0u8; 512];
    b[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    b[3..11].copy_from_slice(b"DATBOI  ");
    b[11..13].copy_from_slice(&u16::try_from(SECTOR).expect("512").to_le_bytes());
    b[13] = u8::try_from(spc).expect("≤128 sectors per cluster");
    b[14..16].copy_from_slice(&u16::try_from(RESERVED_SECTORS).expect("32").to_le_bytes());
    b[16] = 2; // FAT copies
    // root entries (17), total16 (19), fatsz16 (22): zero for FAT32
    b[21] = 0xF8; // media
    b[24..26].copy_from_slice(&63u16.to_le_bytes()); // sectors/track (CHS relic)
    b[26..28].copy_from_slice(&255u16.to_le_bytes()); // heads
    let hidden = if params.partition {
        u32::try_from(PARTITION_LBA).expect("2048")
    } else {
        0
    };
    b[28..32].copy_from_slice(&hidden.to_le_bytes());
    b[32..36].copy_from_slice(&u32::try_from(total_sectors).expect("checked").to_le_bytes());
    b[36..40].copy_from_slice(
        &u32::try_from(fat_sectors)
            .expect("fits total")
            .to_le_bytes(),
    );
    // ext flags (40), fs version (42): zero — mirrored FATs
    b[44..48].copy_from_slice(&2u32.to_le_bytes()); // root cluster
    b[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo sector
    b[50..52].copy_from_slice(&6u16.to_le_bytes()); // backup boot sector
    b[64] = 0x80; // drive number
    b[66] = 0x29; // extended boot signature
    b[67..71].copy_from_slice(&params.serial.to_le_bytes());
    b[71..82].copy_from_slice(&params.volume_label);
    b[82..90].copy_from_slice(b"FAT32   ");
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}

fn fsinfo_sector(free_clusters: u64, next_free: u32) -> Vec<u8> {
    let mut b = vec![0u8; 512];
    b[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes()); // "RRaA"
    b[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes()); // "rrAa"
    b[488..492].copy_from_slice(
        &u32::try_from(free_clusters)
            .expect("≤ cluster max")
            .to_le_bytes(),
    );
    b[492..496].copy_from_slice(&next_free.to_le_bytes());
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}

fn mbr_sector(params: &Fat32Params, partition_sectors: u64) -> Vec<u8> {
    let mut b = vec![0u8; 512];
    b[0x1B8..0x1BC].copy_from_slice(&params.disk_signature.to_le_bytes());
    let e = 0x1BE;
    // boot flag 0x00; degenerate CHS (LBA-only, the modern convention)
    b[e + 1..e + 4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    b[e + 4] = 0x0C; // FAT32 LBA
    b[e + 5..e + 8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    b[e + 8..e + 12].copy_from_slice(&u32::try_from(PARTITION_LBA).expect("2048").to_le_bytes());
    b[e + 12..e + 16].copy_from_slice(
        &u32::try_from(partition_sectors)
            .expect("checked")
            .to_le_bytes(),
    );
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}

/// Convenience: blake3 of a skeleton buffer (golden-test anchor).
#[must_use]
pub fn skeleton_hash(bytes: &[u8]) -> Blake3 {
    Blake3::compute(bytes)
}

// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str, size: u64) -> FileEntry {
        FileEntry {
            path: path.to_owned(),
            size,
        }
    }

    fn params(cluster_size: u32, partition: bool) -> Fat32Params {
        Fat32Params {
            volume_label: label_for("testvol"),
            serial: 0x1234_5678,
            disk_signature: 0x9ABC_DEF0,
            cluster_size,
            partition,
        }
    }

    fn sample_files() -> Vec<FileEntry> {
        vec![
            f("Alpha (USA).gba", 700), // multi-cluster at cs=512
            f("empty.sav", 0),         // zero-size: first cluster 0
            f("exact.bin", 1024),      // exactly two clusters, no slack
            f("sub/Beta (Europe).gba", 300),
            f("sub/deep/Gamma.gba", 512),
            f("z 2 games in 1.gba", 40), // space-mangled 8.3 → ~1 tail
        ]
    }

    /// Walk segments, returning (absolute offset, segment) pairs.
    fn offsets(l: &Fat32Layout) -> Vec<(u64, &LayoutSegment)> {
        let mut off = 0;
        l.segments
            .iter()
            .map(|s| {
                let o = off;
                off += segment_len(s);
                (o, s)
            })
            .collect()
    }

    fn fat_entry(fat: &[u8], ix: u64) -> u32 {
        let o = usize::try_from(ix * 4).unwrap();
        u32::from_le_bytes(fat[o..o + 4].try_into().unwrap()) & 0x0FFF_FFFF
    }

    #[test]
    fn min_clusters_sums_and_alignment() {
        for partition in [false, true] {
            let l = layout(&sample_files(), &params(512, partition)).unwrap();
            let g = l.geometry;
            assert_eq!(g.n_clusters, 65_525, "small sets pad to the FAT32 minimum");
            assert_eq!(
                l.segments.iter().map(segment_len).sum::<u64>(),
                g.total_size
            );
            assert_eq!(g.partition_offset, if partition { 1 << 20 } else { 0 });
            for (off, s) in offsets(&l) {
                if let LayoutSegment::File { .. } = s {
                    assert_eq!(
                        (off - g.data_offset) % u64::from(g.cluster_size),
                        0,
                        "file segments start cluster-aligned"
                    );
                }
            }
            // Dirs window begins at cluster 2.
            let dirs_off = offsets(&l)
                .iter()
                .find_map(|(o, s)| matches!(s, LayoutSegment::Dirs { .. }).then_some(*o))
                .unwrap();
            assert_eq!(dirs_off, g.data_offset);
        }
    }

    #[test]
    fn fat_chains_are_sequential_and_complete() {
        let files = sample_files();
        let l = layout(&files, &params(512, false)).unwrap();
        let g = l.geometry;
        assert_eq!(fat_entry(&l.fat, 0), 0x0FFF_FFF8 & 0x0FFF_FFFF);
        assert_eq!(fat_entry(&l.fat, 1), EOC);

        // Reconstruct expected allocation: dirs (root, sub, sub/deep)
        // then files in row order.
        let cs = u64::from(g.cluster_size);
        let mut expected_used = g.dir_clusters;
        for fe in &files {
            expected_used += fe.size.div_ceil(cs);
        }
        assert_eq!(g.used_clusters, expected_used);

        // Every used cluster chains n → n+1 within its run and every
        // run ends in EOC; every free cluster is zero.
        let mut run_starts = vec![];
        let mut c = 2u64;
        // dirs: root=1 cluster (label+6 entries incl. LFNs? still ≤16), sub, deep
        // We don't hand-assume sizes; instead walk the FAT itself:
        while c < 2 + g.used_clusters {
            run_starts.push(c);
            let mut cur = c;
            loop {
                let next = fat_entry(&l.fat, cur);
                if next == EOC {
                    c = cur + 1;
                    break;
                }
                assert_eq!(u64::from(next), cur + 1, "chains are strictly sequential");
                cur = u64::from(next);
            }
        }
        for free in (2 + g.used_clusters)..(2 + g.n_clusters) {
            assert_eq!(fat_entry(&l.fat, free), 0, "free clusters are zero");
        }
        // FSInfo free count matches.
        let fsinfo = match &l.segments[1] {
            LayoutSegment::Literal(b) => b.clone(),
            other => panic!("expected FSInfo literal, got {other:?}"),
        };
        let free_count = u32::from_le_bytes(fsinfo[488..492].try_into().unwrap());
        assert_eq!(u64::from(free_count), g.n_clusters - g.used_clusters);
        let next_free = u32::from_le_bytes(fsinfo[492..496].try_into().unwrap());
        assert_eq!(u64::from(next_free), 2 + g.used_clusters);
    }

    /// Minimal read-back of a directory buffer: reconstruct (long name,
    /// attr, first cluster, size) tuples, verifying LFN sequence rules
    /// and checksums along the way.
    fn parse_dir(buf: &[u8]) -> Vec<(String, u8, u32, u32)> {
        let mut out = vec![];
        let mut pending: Vec<(u8, Vec<u16>, u8)> = vec![]; // (seq, units, checksum)
        for e in buf.chunks(32) {
            if e[0] == 0 {
                break;
            }
            if e[11] == 0x0F {
                let seq = e[0] & 0x1F;
                let last = e[0] & 0x40 != 0;
                if last {
                    assert!(pending.is_empty(), "LFN chain restarted mid-chain");
                } else {
                    assert_eq!(pending.last().unwrap().0, seq + 1, "LFN sequence descends");
                }
                const SLOTS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
                let units: Vec<u16> = SLOTS
                    .iter()
                    .map(|&p| u16::from_le_bytes(e[p..p + 2].try_into().unwrap()))
                    .collect();
                pending.push((seq, units, e[13]));
                continue;
            }
            let short: [u8; 11] = e[..11].try_into().unwrap();
            let attr = e[11];
            let cluster = (u32::from(u16::from_le_bytes(e[20..22].try_into().unwrap())) << 16)
                | u32::from(u16::from_le_bytes(e[26..28].try_into().unwrap()));
            let size = u32::from_le_bytes(e[28..32].try_into().unwrap());
            let name = if pending.is_empty() {
                let stem = String::from_utf8_lossy(&short[..8]).trim_end().to_owned();
                let ext = String::from_utf8_lossy(&short[8..]).trim_end().to_owned();
                if ext.is_empty() {
                    stem
                } else {
                    format!("{stem}.{ext}")
                }
            } else {
                let ck = sfn_checksum(&short);
                let mut units = vec![];
                for (seq, u, c) in pending.drain(..).rev() {
                    assert_eq!(c, ck, "LFN checksum matches following SFN");
                    assert!(seq >= 1);
                    units.extend(u);
                }
                let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
                String::from_utf16(&units[..end]).unwrap()
            };
            out.push((name, attr, cluster, size));
        }
        assert!(pending.is_empty(), "dangling LFN entries");
        out
    }

    #[test]
    fn directory_entries_reconstruct_the_tree() {
        let files = sample_files();
        let l = layout(&files, &params(512, false)).unwrap();
        let cs = usize::try_from(l.geometry.cluster_size).unwrap();

        // Root: label + children.
        let root = parse_dir(&l.dirs);
        assert_eq!(root[0].1, ATTR_VOLUME_ID);
        assert_eq!(&root[0].0, "TESTVOL");
        let names: Vec<&str> = root[1..].iter().map(|(n, ..)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Alpha (USA).gba",
                "empty.sav",
                "exact.bin",
                "sub",
                "z 2 games in 1.gba"
            ]
        );
        // Zero-size file: cluster 0, size 0.
        let empty = root.iter().find(|(n, ..)| n == "empty.sav").unwrap();
        assert_eq!((empty.2, empty.3), (0, 0));
        // Sizes recorded.
        let alpha = root.iter().find(|(n, ..)| n == "Alpha (USA).gba").unwrap();
        assert_eq!(alpha.3, 700);

        // Subdir `sub`: dot entries then children.
        let sub = root.iter().find(|(n, ..)| n == "sub").unwrap();
        assert_eq!(sub.1, ATTR_DIRECTORY);
        let sub_first = sub.2;
        // Dir buffer offset: clusters are packed in allocation order
        // starting at cluster 2.
        let sub_off = usize::try_from(sub_first - 2).unwrap() * cs;
        let sub_entries = parse_dir(&l.dirs[sub_off..]);
        assert_eq!(sub_entries[0].0, ".");
        assert_eq!(sub_entries[1].0, "..");
        assert_eq!(sub_entries[1].2, 0, ".. of a root child points at 0");
        let sub_names: Vec<&str> = sub_entries[2..].iter().map(|(n, ..)| n.as_str()).collect();
        assert_eq!(sub_names, vec!["Beta (Europe).gba", "deep"]);

        let deep = sub_entries.iter().find(|(n, ..)| n == "deep").unwrap();
        let deep_off = usize::try_from(deep.2 - 2).unwrap() * cs;
        let deep_entries = parse_dir(&l.dirs[deep_off..]);
        assert_eq!(deep_entries[1].2, sub_first, ".. points at parent");
        assert_eq!(deep_entries[2].0, "Gamma.gba");
    }

    #[test]
    fn short_name_tails_are_deterministic() {
        let mut used = std::collections::HashSet::new();
        let (a, lfn_a) = short_name("hello world.gba", &mut used);
        let (b, lfn_b) = short_name("hello worle.gba", &mut used);
        assert!(lfn_a && lfn_b);
        // Space dropped, stem truncated to make room for the tail;
        // same base collides, so the second name takes ~2.
        assert_eq!(&a, b"HELLOW~1GBA");
        assert_eq!(&b, b"HELLOW~2GBA");
        // Pure 8.3 names need no LFN and no tail.
        let (c, lfn_c) = short_name("EXACT.BIN", &mut used);
        assert_eq!(&c, b"EXACT   BIN");
        assert!(!lfn_c);
        // Case-only difference: no tail, but LFN preserved.
        let (d, lfn_d) = short_name("Other.bin", &mut used);
        assert_eq!(&d, b"OTHER   BIN");
        assert!(lfn_d);
    }

    #[test]
    fn boot_sector_invariants_and_backup() {
        let l = layout(&sample_files(), &params(512, false)).unwrap();
        let boot = match &l.segments[0] {
            LayoutSegment::Literal(b) => b.clone(),
            other => panic!("expected boot literal, got {other:?}"),
        };
        assert_eq!(&boot[510..], &[0x55, 0xAA]);
        assert_eq!(u16::from_le_bytes(boot[11..13].try_into().unwrap()), 512);
        let spc = u64::from(boot[13]);
        let reserved = u64::from(u16::from_le_bytes(boot[14..16].try_into().unwrap()));
        let fatsz = u64::from(u32::from_le_bytes(boot[36..40].try_into().unwrap()));
        let totsec = u64::from(u32::from_le_bytes(boot[32..36].try_into().unwrap()));
        assert_eq!(
            totsec,
            reserved + 2 * fatsz + l.geometry.n_clusters * spc,
            "sector count exactly covers reserved + FATs + clusters"
        );
        assert_eq!(
            u32::from_le_bytes(boot[67..71].try_into().unwrap()),
            0x1234_5678
        );
        assert_eq!(&boot[71..82], &label_for("testvol"));
        // Backup boot (segment index 3 in the superfloppy shape) is identical.
        assert_eq!(l.segments[3], l.segments[0]);
        assert_eq!(l.segments[4], l.segments[1]);
    }

    #[test]
    fn mbr_invariants() {
        let l = layout(&sample_files(), &params(512, true)).unwrap();
        let mbr = match &l.segments[0] {
            LayoutSegment::Literal(b) => b.clone(),
            other => panic!("expected MBR literal, got {other:?}"),
        };
        assert_eq!(&mbr[510..], &[0x55, 0xAA]);
        assert_eq!(
            u32::from_le_bytes(mbr[0x1B8..0x1BC].try_into().unwrap()),
            0x9ABC_DEF0
        );
        assert_eq!(mbr[0x1BE + 4], 0x0C);
        assert_eq!(
            u32::from_le_bytes(mbr[0x1BE + 8..0x1BE + 12].try_into().unwrap()),
            2048
        );
        let count = u64::from(u32::from_le_bytes(
            mbr[0x1BE + 12..0x1BE + 16].try_into().unwrap(),
        ));
        assert_eq!((l.geometry.total_size - (1 << 20)) / SECTOR, count);
        // Hidden sectors in the boot sector reflect the partition start.
        let boot = match &l.segments[2] {
            LayoutSegment::Literal(b) => b.clone(),
            other => panic!("expected boot literal, got {other:?}"),
        };
        assert_eq!(u32::from_le_bytes(boot[28..32].try_into().unwrap()), 2048);
    }

    #[test]
    fn refusals() {
        let p = params(512, false);
        assert!(matches!(
            layout(&[f("a", 1), f("a/b", 1)], &p),
            Err(Fat32Error::PathConflict(_))
        ));
        assert!(matches!(
            layout(&[f("Foo.gba", 1), f("foo.gba", 1)], &p),
            Err(Fat32Error::PathConflict(_))
        ));
        assert!(matches!(
            layout(&[f("b.gba", 1), f("a.gba", 1)], &p),
            Err(Fat32Error::Unsorted(_))
        ));
        assert!(matches!(
            layout(&[f("big.iso", 4 << 30)], &p),
            Err(Fat32Error::FileTooLarge { .. })
        ));
        assert!(matches!(
            layout(&[f("bad:name.gba", 1)], &p),
            Err(Fat32Error::BadPath(_))
        ));
        assert!(matches!(
            layout(
                &[],
                &Fat32Params {
                    cluster_size: 700,
                    ..params(512, false)
                }
            ),
            Err(Fat32Error::BadClusterSize(700))
        ));
    }

    #[test]
    fn empty_view_is_a_valid_volume() {
        let l = layout(&[], &params(512, false)).unwrap();
        assert_eq!(l.geometry.n_clusters, 65_525);
        assert_eq!(l.geometry.used_clusters, 1, "root directory only");
        let root = parse_dir(&l.dirs);
        assert_eq!(root.len(), 1, "just the volume label");
    }

    /// Format commitment: the skeleton bytes for a pinned input are
    /// golden. If this breaks, the on-disk layout changed — that is a
    /// format event, same rank as the viewsnap golden (D62).
    #[test]
    fn golden_skeletons() {
        let l = layout(&sample_files(), &params(512, true)).unwrap();
        // 1 MiB partition gap + 32 reserved + 2×512 FAT + 65,525 data sectors.
        assert_eq!(l.geometry.total_size, 35_138_048);
        assert_eq!(
            skeleton_hash(&l.fat).to_hex(),
            "62d10caa36708c465f232c70d3ca781982ba0696353528547487405ad8993ede"
        );
        assert_eq!(
            skeleton_hash(&l.dirs).to_hex(),
            "cd905afb36f0089d28f7fbc156036e2504c6871a7b32afd467fffbf1ca41c874"
        );
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        fn name_strategy() -> impl Strategy<Value = String> {
            // FAT-legal, mixed case, occasionally 8.3-unfriendly.
            proptest::string::string_regex("[A-Za-z0-9_ ()!'-]{1,24}(\\.[A-Za-z0-9]{1,5})?")
                .expect("valid regex")
                .prop_filter("no trailing dot/space", |s| component_is_fat_legal(s))
        }

        fn files_strategy() -> impl Strategy<Value = Vec<FileEntry>> {
            let entry = (
                proptest::collection::vec(name_strategy(), 1..=3),
                0u64..5000,
            );
            proptest::collection::vec(entry, 0..20).prop_map(|v| {
                let mut files: Vec<FileEntry> = v
                    .into_iter()
                    .map(|(comps, size)| FileEntry {
                        path: comps.join("/"),
                        size,
                    })
                    .collect();
                files.sort_by(|a, b| a.path.cmp(&b.path));
                files.dedup_by(|a, b| a.path == b.path);
                files
            })
        }

        proptest! {
            #[test]
            fn layout_invariants_hold(files in files_strategy(), partition in any::<bool>()) {
                let p = params(512, partition);
                // Case-insensitive collisions are a legal refusal, not a bug.
                let l = match layout(&files, &p) {
                    Ok(l) => l,
                    Err(Fat32Error::PathConflict(_)) => return Ok(()),
                    Err(e) => panic!("unexpected error: {e}"),
                };
                let g = l.geometry;
                prop_assert!(g.n_clusters >= 65_525);
                prop_assert_eq!(
                    l.segments.iter().map(segment_len).sum::<u64>(),
                    g.total_size
                );
                let mut off = 0u64;
                for s in &l.segments {
                    if let LayoutSegment::File { .. } = s {
                        prop_assert_eq!((off - g.data_offset) % u64::from(g.cluster_size), 0);
                    }
                    off += segment_len(s);
                }
                // Bit determinism.
                let l2 = layout(&files, &p).expect("same input");
                prop_assert_eq!(l.fat, l2.fat);
                prop_assert_eq!(l.dirs, l2.dirs);
                prop_assert_eq!(l.segments, l2.segments);
            }
        }
    }
}
