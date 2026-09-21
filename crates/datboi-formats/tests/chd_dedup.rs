//! The measurement that decides whether CHD decomposition is worth
//! building — ignored by default, because it needs a real corpus.
//!
//! ```text
//! DATBOI_CHD_DIR=/path/to/chds \
//!   cargo test -p datboi-formats --test chd_dedup -- --ignored --nocapture
//! ```
//!
//! Optional knobs:
//! - `DATBOI_CHD_LIMIT=N` — stop after N files (a sample run).
//! - `DATBOI_CHD_DECOMPRESS=1` — also measure the DECOMPRESSED hunks.
//!   Far slower (it is a full verify of every file) but it is the
//!   number that prices the other design.
//!
//! **What is being asked.** The obvious decomposition of a CHD is
//! hunk-as-blob plus a reassembly recipe. But a CHD's hunks are each
//! compressed on their own, so two CHDs share a hunk blob only if the
//! same bytes were compressed identically — same codec, same settings,
//! same boundaries. That is plausible for one disc dumped twice and
//! implausible across regions, and the difference decides whether the
//! feature exists at all.
//!
//! **What the numbers mean.**
//! - *within-file* dedup is what CHD's own `SELF` map entries already
//!   capture for free; a hunk store would win nothing there.
//! - *cross-file* dedup is the ONLY new saving a hunk store buys. It
//!   is the number to look at.
//! - whole-file duplicates are excluded up front: identical `.chd`
//!   files already dedup at the blob level and would flatter any
//!   hunk-level result that counted them.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use datboi_formats::chd::{self, ChdReader, HunkKind};

#[derive(Default)]
struct Tally {
    hunks: u64,
    bytes: u64,
    /// digest -> stored length, deduplicated globally.
    distinct: HashMap<[u8; 32], u64>,
    /// Hunks that repeat WITHIN their own file (what `SELF` already
    /// covers), so the cross-file saving can be read off separately.
    within_file_repeat_bytes: u64,
    per_codec: HashMap<String, (u64, u64)>,
}

impl Tally {
    fn add(
        &mut self,
        codec: &str,
        digest: [u8; 32],
        len: u64,
        seen_in_file: &mut HashSet<[u8; 32]>,
    ) {
        self.hunks += 1;
        self.bytes += len;
        if !seen_in_file.insert(digest) {
            self.within_file_repeat_bytes += len;
        }
        self.distinct.entry(digest).or_insert(len);
        let e = self.per_codec.entry(codec.to_string()).or_default();
        e.0 += 1;
        e.1 += len;
    }

    fn distinct_bytes(&self) -> u64 {
        self.distinct.values().sum()
    }

    fn report(&self, label: &str) {
        let distinct = self.distinct_bytes();
        let saved = self.bytes.saturating_sub(distinct);
        let cross = saved.saturating_sub(self.within_file_repeat_bytes);
        println!("\n== {label} ==");
        println!(
            "  hunks              {:>14}  ({} distinct)",
            self.hunks,
            self.distinct.len()
        );
        println!("  bytes              {:>14}", self.bytes);
        println!("  distinct bytes     {:>14}", distinct);
        println!(
            "  dedup ratio        {:>14.4}x",
            self.bytes as f64 / distinct.max(1) as f64
        );
        println!(
            "  saved within files {:>14}   (CHD SELF entries already cover this)",
            self.within_file_repeat_bytes
        );
        println!(
            "  saved ACROSS files {:>14}   ({:.3}% of all hunk bytes)",
            cross,
            100.0 * cross as f64 / self.bytes.max(1) as f64
        );
        let mut codecs: Vec<_> = self.per_codec.iter().collect();
        codecs.sort_by_key(|(name, _)| name.as_str());
        for (name, (hunks, bytes)) in codecs {
            println!("  codec {name:<6} {hunks:>10} hunks {bytes:>14} bytes");
        }
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("chd"))
        {
            out.push(path);
        }
    }
}

/// Everything one walk of a corpus produced.
#[derive(Default)]
struct Measurement {
    compressed: Tally,
    plain: Tally,
    total_file_bytes: u64,
    skipped_duplicate_files: u64,
    refused: Vec<(PathBuf, String)>,
}

#[allow(clippy::too_many_lines)]
fn walk(files: &[PathBuf], decompress: bool) -> Measurement {
    let mut compressed = Tally::default();
    let mut plain = Tally::default();
    // Identical files already dedup as whole blobs; counting them here
    // would flatter the hunk-level result with a saving we already have.
    let mut whole_file_digests = HashSet::new();
    let mut skipped_duplicate_files = 0u64;
    let mut refused: Vec<(PathBuf, String)> = Vec::new();
    let mut total_file_bytes = 0u64;

    for path in files {
        let (Ok(mut file), Ok(raw_handle)) = (std::fs::File::open(path), std::fs::File::open(path))
        else {
            continue;
        };
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        total_file_bytes += size;

        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            match file.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    hasher.update(&buf[..n]);
                }
            }
        }
        if !whole_file_digests.insert(*hasher.finalize().as_bytes()) {
            skipped_duplicate_files += 1;
            continue;
        }

        // Two handles: the reader owns one (it seeks as it decodes),
        // and the raw hunk reads below use the other.
        let mut raw_handle = raw_handle;
        let mut reader = match ChdReader::open(file) {
            Ok(r) => r,
            Err(e) => {
                refused.push((path.clone(), e.to_string()));
                continue;
            }
        };
        let hunk_bytes = reader.header().hunk_bytes as usize;
        let entries: Vec<_> = reader.map().to_vec();

        let mut seen_compressed = HashSet::new();
        let mut seen_plain = HashSet::new();
        let mut hunk = vec![0u8; hunk_bytes];
        for (index, entry) in entries.iter().enumerate() {
            let codec = match entry.kind {
                HunkKind::Codec(c) => chd::codec_name(c),
                HunkKind::Uncompressed => "raw".into(),
                // Structural entries store no bytes of their own —
                // they are already the dedup CHD does for itself.
                _ => continue,
            };
            let len = u64::from(entry.length);
            let mut raw = vec![0u8; entry.length as usize];
            if raw_handle.seek(SeekFrom::Start(entry.offset)).is_err()
                || raw_handle.read_exact(&mut raw).is_err()
            {
                refused.push((path.clone(), format!("hunk {index} is not in the file")));
                break;
            }
            compressed.add(
                &codec,
                *blake3::hash(&raw).as_bytes(),
                len,
                &mut seen_compressed,
            );

            if decompress {
                match reader.read_hunk(u32::try_from(index).expect("hunk index"), &mut hunk) {
                    Ok(()) => plain.add(
                        &codec,
                        *blake3::hash(&hunk).as_bytes(),
                        hunk_bytes as u64,
                        &mut seen_plain,
                    ),
                    Err(e) => {
                        refused.push((path.clone(), format!("hunk {index}: {e}")));
                        break;
                    }
                }
            }
        }
    }

    Measurement {
        compressed,
        plain,
        total_file_bytes,
        skipped_duplicate_files,
        refused,
    }
}

#[test]
#[ignore = "needs a real CHD corpus; see the module docs"]
fn measure_hunk_dedup() {
    let Ok(root) = std::env::var("DATBOI_CHD_DIR") else {
        panic!("set DATBOI_CHD_DIR to a directory of .chd files");
    };
    let limit = std::env::var("DATBOI_CHD_LIMIT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(usize::MAX);
    let decompress = std::env::var("DATBOI_CHD_DECOMPRESS").is_ok();

    let mut files = Vec::new();
    collect(Path::new(&root), &mut files);
    files.sort();
    files.truncate(limit);
    println!("{} CHD files under {root}", files.len());

    let Measurement {
        compressed,
        plain,
        total_file_bytes,
        skipped_duplicate_files,
        refused,
    } = walk(&files, decompress);

    println!("\n{total_file_bytes} bytes of .chd on disk");
    println!("{skipped_duplicate_files} byte-identical file(s) skipped (already blob-deduped)");
    compressed.report("compressed hunks as stored (the hunk-as-blob design)");
    if decompress {
        plain.report("decompressed hunks (prices the store-inflated design)");
        println!(
            "\n  inflating the corpus would cost {} bytes resident vs {} today ({:.2}x)",
            plain.distinct_bytes(),
            total_file_bytes,
            plain.distinct_bytes() as f64 / total_file_bytes.max(1) as f64
        );
    }
    if !refused.is_empty() {
        println!("\n{} file(s) this build could not walk:", refused.len());
        for (path, why) in refused.iter().take(40) {
            println!("  {}: {why}", path.display());
        }
    }
}

/// The harness is only worth handing to an operator if it can be shown
/// to count correctly, so it is exercised on CHDs built here: two files
/// that share an aligned prefix (the best case a hunk store could ever
/// have — one disc, two dumps by the same tool) and one that shares
/// nothing.
#[test]
fn the_harness_counts_shared_hunks_and_ignores_unshared_ones() {
    use datboi_formats::chd::synth::{SynthSpec, build};

    /// Compressible but never self-repeating: a narrow value range (so
    /// every codec beats storing it raw) over a full-period PRNG (so no
    /// two hunks of one file are accidentally identical, which would
    /// make the within-file column meaningless).
    fn wave(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 2);
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        while out.len() < len {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let v = i16::try_from((x >> 33) % 512).expect("range") - 256;
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.truncate(len);
        out
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let spec = SynthSpec::new(5, chd::CODEC_ZLIB);
    let hunk = spec.hunk_bytes as usize;

    // Two discs sharing their first four hunks exactly, then diverging.
    let shared = wave(hunk * 4, 0);
    let mut a = shared.clone();
    a.extend_from_slice(&wave(hunk * 2, 11));
    let mut b = shared;
    b.extend_from_slice(&wave(hunk * 2, 97));
    // And one with nothing in common.
    let c = wave(hunk * 3, 250);

    let mut files = Vec::new();
    for (name, data) in [("a.chd", &a), ("b.chd", &b), ("c.chd", &c)] {
        let path = dir.path().join(name);
        std::fs::write(&path, build(&spec, data)).expect("write");
        files.push(path);
    }
    files.sort();

    let m = walk(&files, true);
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    assert_eq!(m.skipped_duplicate_files, 0);
    assert_eq!(m.compressed.hunks, 4 + 2 + 4 + 2 + 3);

    // Four hunks of `a` are byte-identical to four of `b`, in both the
    // compressed and the decompressed view — so both tallies must see
    // exactly four hunks' worth of cross-file saving.
    for (label, tally, unit) in [
        ("compressed", &m.compressed, None),
        ("decompressed", &m.plain, Some(hunk as u64)),
    ] {
        let saved = tally.bytes - tally.distinct_bytes();
        assert_eq!(
            tally.within_file_repeat_bytes, 0,
            "{label}: no hunk repeats inside one of these files"
        );
        assert!(saved > 0, "{label}: the shared prefix must be counted");
        assert_eq!(
            tally.distinct.len(),
            4 + 2 + 2 + 3,
            "{label}: the four shared hunks collapse to one copy"
        );
        if let Some(unit) = unit {
            assert_eq!(saved, 4 * unit, "{label}");
        }
    }
}
