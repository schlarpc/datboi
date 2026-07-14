//! Random-access adapters for operator-tree nodes. The vocabulary trait
//! is [`datboi_runtime::stream::RangeRead`] — the same shape the @2 wasm
//! host feeds to guests — so a node is "seekable" exactly when it can
//! hand out one of these.

use std::fs::File;
use std::io::{self, Read};

use datboi_core::assemble::{AssembleParams, Piece, translate};
use datboi_core::hash::Blake3;
use datboi_runtime::stream::RangeRead;
use datboi_store_fs::obao;

/// Fill `buf` completely from `src` at `offset`; erroring if the source
/// ends first (callers have already validated bounds).
pub fn read_at_exact(src: &mut dyn RangeRead, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match src.read_at(offset + filled as u64, &mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "source shorter than validated bounds",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub use datboi_runtime::stream::FileRandom;

/// A resident literal leaf served through its OWN bao tree: every
/// `read_at` re-validates the covering 16 KiB chunk groups against the
/// leaf hash before returning bytes. The D63 carve-out's leaf reader —
/// input-side verification standing in for the missing output bao.
pub struct VerifiedRandom {
    file: File,
    len: u64,
    hash: Blake3,
    /// Pre-order obao4; empty is correct for blobs ≤ one chunk group.
    sidecar: Vec<u8>,
}

impl VerifiedRandom {
    #[must_use]
    pub fn new(file: File, len: u64, hash: Blake3, sidecar: Vec<u8>) -> Self {
        Self {
            file,
            len,
            hash,
            sidecar,
        }
    }
}

impl RangeRead for VerifiedRandom {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let end = offset.saturating_add(buf.len() as u64).min(self.len);
        let bytes =
            obao::read_range_verified(&self.file, self.len, &self.hash, &self.sidecar, offset..end)
                .map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("verified leaf {}: {e}", self.hash),
                    )
                })?;
        buf[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }

    fn len(&self) -> u64 {
        self.len
    }
}

/// Affine assemble node as a random-access source: range reads translate
/// arithmetically onto the children (docs/recipes.md execution).
pub struct AssembleRandom {
    params: AssembleParams,
    size: u64,
    children: Vec<Box<dyn RangeRead>>,
}

impl AssembleRandom {
    /// # Errors
    /// If the params are structurally invalid or a segment exceeds its
    /// child's length.
    pub fn new(params: AssembleParams, children: Vec<Box<dyn RangeRead>>) -> Result<Self, String> {
        let size = params.output_size().map_err(|e| e.to_string())?;
        for (ix, segment) in params.segments.iter().enumerate() {
            if let datboi_core::assemble::Segment::BlobRange {
                input_ix,
                offset,
                len,
            } = segment
            {
                let child = children
                    .get(*input_ix as usize)
                    .ok_or_else(|| format!("segment {ix}: input {input_ix} out of range"))?;
                if offset.checked_add(*len).is_none_or(|end| end > child.len()) {
                    return Err(format!(
                        "segment {ix}: {offset}+{len} exceeds input length {}",
                        child.len()
                    ));
                }
            }
        }
        Ok(Self {
            params,
            size,
            children,
        })
    }
}

impl RangeRead for AssembleRandom {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = offset.min(self.size);
        let end = offset.saturating_add(buf.len() as u64).min(self.size);
        if start == end {
            return Ok(0);
        }
        let pieces =
            translate(&self.params, start..end).map_err(|e| io::Error::other(e.to_string()))?;
        let mut filled = 0usize;
        for piece in pieces {
            match piece {
                Piece::Input {
                    input_ix,
                    offset,
                    len,
                } => {
                    let n = usize::try_from(len).expect("bounded by buf");
                    read_at_exact(
                        self.children[input_ix as usize].as_mut(),
                        offset,
                        &mut buf[filled..filled + n],
                    )?;
                    filled += n;
                }
                Piece::Fill { byte, len } => {
                    let n = usize::try_from(len).expect("bounded by buf");
                    buf[filled..filled + n].fill(byte);
                    filled += n;
                }
                Piece::Literal(bytes) => {
                    buf[filled..filled + bytes.len()].copy_from_slice(bytes);
                    filled += bytes.len();
                }
            }
        }
        Ok(filled)
    }

    fn len(&self) -> u64 {
        self.size
    }
}

/// Sequential façade over any random-access source.
pub struct SeqOverRandom {
    inner: Box<dyn RangeRead>,
    pos: u64,
}

impl SeqOverRandom {
    #[must_use]
    pub fn new(inner: Box<dyn RangeRead>) -> Self {
        Self { inner, pos: 0 }
    }
}

impl Read for SeqOverRandom {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read_at(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// Sequential window `[start, start+len)` over a random-access source —
/// the deflate-decompress input shape (a zip member's compressed span).
pub struct WindowSeq {
    src: Box<dyn RangeRead>,
    start: u64,
    remaining: u64,
}

impl WindowSeq {
    #[must_use]
    pub fn new(src: Box<dyn RangeRead>, start: u64, len: u64) -> Self {
        Self {
            src,
            start,
            remaining: len,
        }
    }
}

impl Read for WindowSeq {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let cap = usize::try_from(self.remaining.min(buf.len() as u64)).expect("bounded");
        let n = self.src.read_at(self.start, &mut buf[..cap])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "window extends past end of source",
            ));
        }
        self.start += n as u64;
        self.remaining -= n as u64;
        Ok(n)
    }
}
