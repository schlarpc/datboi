//! bao outboard machinery (docs/cas.md, D49): the blake3 hash tree
//! that makes every future range read verifiable without
//! rematerialization.
//!
//! Format: headerless pre-order obao4 — 16 KiB chunk groups
//! (`BlockSize(4)`), hash pairs only, blob length implied by the data
//! file. This is byte-identical to what iroh-blobs writes, so the M6 p2p
//! layer serves these sidecars unchanged (D2/D14 alignment). Ratified as
//! D52 — a decades-scale at-rest commitment like the store layout
//! itself, and since D105 also frozen INTO the pack format (packs carry
//! member trees in their outboard section).
//!
//! Outboards are self-authenticating against the blob hash (the file
//! name): a corrupt, truncated, or foreign sidecar makes validation fail —
//! it can never cause wrong bytes to verify. That is why peer-supplied
//! outboards need no trust machinery (D49 rule 1).
//!
//! Blobs of one chunk group or less (≤ 16 KiB) have an EMPTY outboard
//! (the root hash IS the group hash), so no sidecar exists below that
//! threshold and verification degenerates to hashing the whole blob.

use std::io::Read;
use std::ops::Range;

use bao_tree::io::outboard::PreOrderMemOutboard;
use bao_tree::io::sync::{outboard as fill_outboard, valid_ranges};
use bao_tree::{BaoTree, BlockSize, ChunkNum, ChunkRanges};
use datboi_core::hash::Blake3;
use positioned_io::ReadAt;

/// obao4: 16 KiB chunk groups, matching iroh-blobs.
pub const BLOCK_SIZE: BlockSize = BlockSize::from_chunk_log(4);

/// Bytes covered by one chunk group; blobs at or under this need no
/// sidecar (their outboard is empty).
pub const GROUP_BYTES: u64 = 16 * 1024;

/// Sidecar size in bytes for a blob of `len` bytes (0 for ≤ one group).
#[must_use]
pub fn outboard_size(len: u64) -> u64 {
    BaoTree::new(len, BLOCK_SIZE).outboard_size()
}

#[derive(Debug, thiserror::Error)]
pub enum ObaoError {
    #[error("outboard i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("outboard sidecar is {actual} bytes, tree for {len} bytes needs {expected}")]
    SidecarSize {
        actual: u64,
        expected: u64,
        len: u64,
    },
    /// The requested range failed hash-tree validation: on-disk bytes do
    /// not match the blob hash. Serving surfaces translate this to EIO —
    /// bad bytes are never returned (D49 rule 2).
    #[error("range {start}..{end} failed bao validation against {hash}")]
    RangeInvalid { hash: Blake3, start: u64, end: u64 },
}

/// Compute the pre-order outboard for exactly `len` bytes streamed from
/// `reader`, returning `(root, sidecar bytes)`. One sequential pass;
/// memory is O(tree): 64 bytes per 16 KiB of data, ~16 MiB at 4 GiB.
///
/// The caller compares `root` against the expected blob hash — a short or
/// long reader surfaces as a root mismatch, never a silent success.
///
/// # Errors
/// On read failure, or if `reader` ends before `len` bytes.
pub fn compute(reader: impl Read, len: u64) -> Result<(Blake3, Vec<u8>), ObaoError> {
    let tree = BaoTree::new(len, BLOCK_SIZE);
    let size = usize::try_from(tree.outboard_size()).expect("outboard fits in memory");
    let mut ob = PreOrderMemOutboard {
        root: blake3::Hash::from_bytes([0u8; 32]),
        tree,
        data: vec![0u8; size],
    };
    let root = fill_outboard(reader, tree, &mut ob)?;
    Ok((Blake3(*root.as_bytes()), ob.data))
}

/// Validate `range` (byte offsets) of `data` against `hash` using the
/// pre-order `sidecar`, then read and return exactly those bytes.
///
/// The whole covering set of 16 KiB chunk groups is re-hashed on every
/// call — that is the D49 mandate for recipe-served reads, not an
/// accident. Blobs are immutable under the single-writer daemon, so the
/// validate-then-read pair is not a TOCTOU surface.
///
/// `range` must already be clamped to `len`.
///
/// # Errors
/// [`ObaoError::RangeInvalid`] when validation fails; sidecar-size and
/// I/O errors as their variants.
pub fn read_range_verified(
    data: impl ReadAt,
    len: u64,
    hash: &Blake3,
    sidecar: &[u8],
    range: Range<u64>,
) -> Result<Vec<u8>, ObaoError> {
    assert!(
        range.start <= range.end && range.end <= len,
        "caller clamps"
    );
    let tree = BaoTree::new(len, BLOCK_SIZE);
    if sidecar.len() as u64 != tree.outboard_size() {
        return Err(ObaoError::SidecarSize {
            actual: sidecar.len() as u64,
            expected: tree.outboard_size(),
            len,
        });
    }
    if range.start == range.end {
        return Ok(Vec::new());
    }
    let outboard = PreOrderMemOutboard {
        root: blake3::Hash::from_bytes(hash.0),
        tree,
        data: sidecar,
    };
    // Chunks (1 KiB units) covering the byte range.
    let want = ChunkRanges::from(ChunkNum::full_chunks(range.start)..ChunkNum::chunks(range.end));
    let mut valid = ChunkRanges::empty();
    for item in valid_ranges(outboard, &data, &want) {
        valid |= ChunkRanges::from(item?);
    }
    if !valid.is_superset(&want) {
        return Err(ObaoError::RangeInvalid {
            hash: *hash,
            start: range.start,
            end: range.end,
        });
    }
    let mut buf = vec![0u8; usize::try_from(range.end - range.start).expect("range fits memory")];
    read_exact_at(&data, range.start, &mut buf)?;
    Ok(buf)
}

/// Verify a produced byte window against the blob's outboard WITHOUT the
/// blob on disk — the D49 rule-2 check for recipe-served ranges: the
/// executor produces `window` (which claims to be bytes
/// `window_start .. window_start + window.len()` of the blob) and this
/// proves or refutes it against the hash tree.
///
/// `window_start` must be chunk-group aligned and the window must extend
/// to a group boundary or to `total_len` — validation hashes whole
/// groups, so a ragged window would read outside itself and fail.
///
/// # Errors
/// [`ObaoError::RangeInvalid`] when the window's bytes are not the blob's
/// bytes; sidecar-size errors as their variant.
pub fn verify_window(
    total_len: u64,
    hash: &Blake3,
    sidecar: &[u8],
    window_start: u64,
    window: &[u8],
) -> Result<(), ObaoError> {
    let window_end = window_start + window.len() as u64;
    assert!(
        window_start.is_multiple_of(GROUP_BYTES)
            && (window_end.is_multiple_of(GROUP_BYTES) || window_end == total_len)
            && window_end <= total_len,
        "caller aligns the window to chunk groups"
    );
    let tree = BaoTree::new(total_len, BLOCK_SIZE);
    if sidecar.len() as u64 != tree.outboard_size() {
        return Err(ObaoError::SidecarSize {
            actual: sidecar.len() as u64,
            expected: tree.outboard_size(),
            len: total_len,
        });
    }
    if window.is_empty() {
        return Ok(());
    }
    let outboard = PreOrderMemOutboard {
        root: blake3::Hash::from_bytes(hash.0),
        tree,
        data: sidecar,
    };
    let want = ChunkRanges::from(ChunkNum::full_chunks(window_start)..ChunkNum::chunks(window_end));
    let data = WindowAt {
        start: window_start,
        bytes: window,
    };
    let mut valid = ChunkRanges::empty();
    for item in valid_ranges(outboard, &data, &want) {
        // Validation reads that fail (outside the window) mean invalid,
        // not I/O trouble: the window did not cover what it claimed.
        match item {
            Ok(range) => valid |= ChunkRanges::from(range),
            Err(_) => break,
        }
    }
    if valid.is_superset(&want) {
        Ok(())
    } else {
        Err(ObaoError::RangeInvalid {
            hash: *hash,
            start: window_start,
            end: window_end,
        })
    }
}

/// A byte window sitting at an absolute blob offset, as a positional
/// source. Reads outside the window error — the validator must never see
/// fabricated bytes.
struct WindowAt<'a> {
    start: u64,
    bytes: &'a [u8],
}

impl ReadAt for WindowAt<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let end = self.start + self.bytes.len() as u64;
        if offset < self.start || offset > end {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "read outside produced window",
            ));
        }
        let within = usize::try_from(offset - self.start).expect("window fits memory");
        let n = (self.bytes.len() - within).min(buf.len());
        buf[..n].copy_from_slice(&self.bytes[within..within + n]);
        Ok(n)
    }
}

fn read_exact_at(data: impl ReadAt, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match data.read_at(offset + filled as u64, &mut buf[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "blob shorter than validated range",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn data_of(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn small_blobs_have_empty_outboards() {
        for len in [0usize, 1, 1024, 16 * 1024] {
            let data = data_of(len);
            let (root, sidecar) = compute(data.as_slice(), len as u64).expect("compute");
            assert_eq!(root, Blake3::compute(&data), "root is the blake3 hash");
            assert!(sidecar.is_empty(), "≤ one group needs no sidecar");
            // Verification still works with the empty sidecar.
            let out = read_range_verified(data.as_slice(), len as u64, &root, &[], 0..len as u64)
                .expect("verifies");
            assert_eq!(out, data);
        }
    }

    #[test]
    fn multi_group_round_trip_and_tamper_detection() {
        let len = 100 * 1024 + 17; // 7 groups, ragged tail
        let data = data_of(len);
        let (root, sidecar) = compute(data.as_slice(), len as u64).expect("compute");
        assert_eq!(root, Blake3::compute(&data));
        assert_eq!(sidecar.len() as u64, outboard_size(len as u64));

        let out = read_range_verified(
            data.as_slice(),
            len as u64,
            &root,
            &sidecar,
            5000..90 * 1024 + 3,
        )
        .expect("verifies");
        assert_eq!(out, &data[5000..90 * 1024 + 3]);

        // Flip one byte inside the requested range: validation must fail.
        let mut tampered = data.clone();
        tampered[40 * 1024] ^= 1;
        let err = read_range_verified(
            tampered.as_slice(),
            len as u64,
            &root,
            &sidecar,
            40 * 1024..40 * 1024 + 1,
        )
        .expect_err("tamper detected");
        assert!(matches!(err, ObaoError::RangeInvalid { .. }));

        // A tampered byte OUTSIDE the range doesn't poison the read.
        let out = read_range_verified(tampered.as_slice(), len as u64, &root, &sidecar, 0..1024)
            .expect("unrelated groups still verify");
        assert_eq!(out, &data[..1024]);

        // Truncated sidecar is rejected loudly, not misread.
        let err = read_range_verified(
            data.as_slice(),
            len as u64,
            &root,
            &sidecar[..sidecar.len() - 64],
            0..1024,
        )
        .expect_err("bad sidecar size");
        assert!(matches!(err, ObaoError::SidecarSize { .. }));
    }

    /// FORMAT COMMITMENT (D52): headerless pre-order obao4. If this
    /// golden hash moves, the sidecar format changed — and with it the
    /// D105 pack outboard section.
    #[test]
    fn golden_sidecar() {
        let data = data_of(64 * 1024);
        let (root, sidecar) = compute(data.as_slice(), data.len() as u64).expect("compute");
        assert_eq!(sidecar.len(), 3 * 64, "4 groups → 3 parent nodes");
        assert_eq!(
            Blake3::compute(&sidecar).to_hex(),
            "64f044a9c89de90220352e20f54a47ab6037866f1b8307d84b5b9cacb426f6cd"
        );
        // Root must equal whole-blob blake3 — ties the tree to D2 identity.
        assert_eq!(root, Blake3::compute(&data));
    }

    proptest! {
        #[test]
        fn any_range_verifies_and_matches_slice(
            len in 0u64..200_000,
            split in any::<(u64, u64)>(),
        ) {
            let data = data_of(len as usize);
            let (root, sidecar) = compute(data.as_slice(), len).expect("compute");
            let (a, b) = (split.0 % (len + 1), split.1 % (len + 1));
            let range = a.min(b)..a.max(b);
            let out = read_range_verified(data.as_slice(), len, &root, &sidecar, range.clone())
                .expect("verifies");
            prop_assert_eq!(out, &data[range.start as usize..range.end as usize]);
        }
    }
}
