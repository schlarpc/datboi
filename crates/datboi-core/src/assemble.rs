//! The `assemble@1` builtin (docs/recipes.md): ordered segments over
//! input blobs, fill bytes, and short inline literals. Covers concat,
//! slice, header add, pad, zero-fill, splice, and chunk reassembly, and is
//! fully offset-affine — the seekability spine for filesystem views
//! (docs/views.md).
//!
//! Params encoding (strict canonical CBOR): an array of segment maps.
//! Each variant uses a disjoint key set, so the keys identify the variant:
//!
//! - blob range: `{1: input_ix, 2: offset, 3: len}`
//! - fill:       `{4: byte, 5: len}`
//! - literal:    `{6: bytes}` (1..=4096 bytes)
//!
//! Zero-length segments and empty segment lists are rejected so every
//! logical output has exactly one params encoding (the empty output needs
//! no recipe).

use std::io::{self, Read};
use std::ops::Range;

use crate::cbor::{self, Value};

/// Literals above this must be real blobs so they dedupe (docs/recipes.md).
pub const LITERAL_CAP: usize = 4096;

const SEGKEY_INPUT_IX: u64 = 1;
const SEGKEY_OFFSET: u64 = 2;
const SEGKEY_RANGE_LEN: u64 = 3;
const SEGKEY_FILL_BYTE: u64 = 4;
const SEGKEY_FILL_LEN: u64 = 5;
const SEGKEY_LITERAL: u64 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    BlobRange {
        input_ix: u32,
        offset: u64,
        len: u64,
    },
    Fill {
        byte: u8,
        len: u64,
    },
    Literal {
        bytes: Vec<u8>,
    },
}

impl Segment {
    fn len(&self) -> u64 {
        match self {
            Self::BlobRange { len, .. } | Self::Fill { len, .. } => *len,
            Self::Literal { bytes } => bytes.len() as u64,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembleParams {
    pub segments: Vec<Segment>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AssembleError {
    #[error(transparent)]
    Cbor(#[from] cbor::DecodeError),
    #[error("invalid assemble params: {0}")]
    Params(&'static str),
    #[error("segment {segment}: input index {input_ix} out of range")]
    InputIndex { segment: usize, input_ix: u32 },
    #[error("segment {segment}: {offset}+{len} exceeds source length {source_len}")]
    SourceRange {
        segment: usize,
        offset: u64,
        len: u64,
        source_len: u64,
    },
    #[error("output size overflows u64")]
    SizeOverflow,
    #[error("requested range exceeds output size")]
    RangeOutOfBounds,
}

impl AssembleParams {
    /// Total claimed output size (checked sum of segment lengths).
    pub fn output_size(&self) -> Result<u64, AssembleError> {
        self.segments
            .iter()
            .try_fold(0u64, |acc, s| acc.checked_add(s.len()))
            .ok_or(AssembleError::SizeOverflow)
    }

    pub fn encode(&self) -> Result<Vec<u8>, AssembleError> {
        self.validate()?;
        let items = self
            .segments
            .iter()
            .map(|segment| {
                Value::Map(match segment {
                    Segment::BlobRange {
                        input_ix,
                        offset,
                        len,
                    } => vec![
                        (SEGKEY_INPUT_IX, Value::Uint(u64::from(*input_ix))),
                        (SEGKEY_OFFSET, Value::Uint(*offset)),
                        (SEGKEY_RANGE_LEN, Value::Uint(*len)),
                    ],
                    Segment::Fill { byte, len } => vec![
                        (SEGKEY_FILL_BYTE, Value::Uint(u64::from(*byte))),
                        (SEGKEY_FILL_LEN, Value::Uint(*len)),
                    ],
                    Segment::Literal { bytes } => {
                        vec![(SEGKEY_LITERAL, Value::Bytes(bytes.clone()))]
                    }
                })
            })
            .collect();
        Ok(cbor::encode(&Value::Array(items)).expect("segment keys are distinct constants"))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, AssembleError> {
        let Value::Array(items) = cbor::decode(bytes)? else {
            return Err(AssembleError::Params("expected segment array"));
        };
        let segments = items
            .iter()
            .map(segment_from_value)
            .collect::<Result<Vec<_>, _>>()?;
        let params = Self { segments };
        params.validate()?;
        Ok(params)
    }

    fn validate(&self) -> Result<(), AssembleError> {
        if self.segments.is_empty() {
            return Err(AssembleError::Params("empty segment list"));
        }
        for segment in &self.segments {
            match segment {
                Segment::Literal { bytes } => {
                    if bytes.is_empty() {
                        return Err(AssembleError::Params("empty literal"));
                    }
                    if bytes.len() > LITERAL_CAP {
                        return Err(AssembleError::Params("literal exceeds 4096-byte cap"));
                    }
                }
                _ => {
                    if segment.len() == 0 {
                        return Err(AssembleError::Params("zero-length segment"));
                    }
                }
            }
        }
        Ok(())
    }
}

fn segment_from_value(value: &Value) -> Result<Segment, AssembleError> {
    let Value::Map(entries) = value else {
        return Err(AssembleError::Params("segment must be a map"));
    };
    let keys: Vec<u64> = entries.iter().map(|(k, _)| *k).collect();
    let uint = |key: u64| -> Result<u64, AssembleError> {
        match entries.iter().find(|(k, _)| *k == key) {
            Some((_, Value::Uint(n))) => Ok(*n),
            _ => Err(AssembleError::Params("expected unsigned integer field")),
        }
    };
    match keys.as_slice() {
        [SEGKEY_INPUT_IX, SEGKEY_OFFSET, SEGKEY_RANGE_LEN] => Ok(Segment::BlobRange {
            input_ix: u32::try_from(uint(SEGKEY_INPUT_IX)?)
                .map_err(|_| AssembleError::Params("input index out of range"))?,
            offset: uint(SEGKEY_OFFSET)?,
            len: uint(SEGKEY_RANGE_LEN)?,
        }),
        [SEGKEY_FILL_BYTE, SEGKEY_FILL_LEN] => Ok(Segment::Fill {
            byte: u8::try_from(uint(SEGKEY_FILL_BYTE)?)
                .map_err(|_| AssembleError::Params("fill byte out of range"))?,
            len: uint(SEGKEY_FILL_LEN)?,
        }),
        [SEGKEY_LITERAL] => match &entries[0].1 {
            Value::Bytes(bytes) => Ok(Segment::Literal {
                bytes: bytes.clone(),
            }),
            _ => Err(AssembleError::Params("literal must be a byte string")),
        },
        _ => Err(AssembleError::Params("unrecognized segment key set")),
    }
}

/// Random-access byte source backing an assemble input. The store's blob
/// readers implement this; tests use in-memory slices.
pub trait Source {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fill `buf` exactly, starting at `offset`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

impl Source for &[u8] {
    fn len(&self) -> u64 {
        (**self).len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| io::ErrorKind::UnexpectedEof)?;
        let end = start
            .checked_add(buf.len())
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        let slice = (**self)
            .get(start..end)
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(slice);
        Ok(())
    }
}

/// Validate every segment against the provided sources (index bounds and
/// offset+len within source length).
fn validate_sources<S: Source>(
    params: &AssembleParams,
    sources: &[S],
) -> Result<(), AssembleError> {
    for (segment_ix, segment) in params.segments.iter().enumerate() {
        if let Segment::BlobRange {
            input_ix,
            offset,
            len,
        } = segment
        {
            let source = sources
                .get(*input_ix as usize)
                .ok_or(AssembleError::InputIndex {
                    segment: segment_ix,
                    input_ix: *input_ix,
                })?;
            let end = offset.checked_add(*len);
            if end.is_none_or(|e| e > source.len()) {
                return Err(AssembleError::SourceRange {
                    segment: segment_ix,
                    offset: *offset,
                    len: *len,
                    source_len: source.len(),
                });
            }
        }
    }
    Ok(())
}

/// Streaming reader over the assembled output. Never buffers beyond the
/// caller's read buffer (D-streaming discipline).
pub struct AssembleReader<'a, S: Source> {
    params: &'a AssembleParams,
    sources: &'a [S],
    segment: usize,
    /// Bytes already emitted from the current segment.
    emitted: u64,
}

/// Construct a validated streaming reader for the full output.
pub fn reader<'a, S: Source>(
    params: &'a AssembleParams,
    sources: &'a [S],
) -> Result<AssembleReader<'a, S>, AssembleError> {
    params.validate()?;
    validate_sources(params, sources)?;
    Ok(AssembleReader {
        params,
        sources,
        segment: 0,
        emitted: 0,
    })
}

impl<S: Source> Read for AssembleReader<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(segment) = self.params.segments.get(self.segment) else {
                return Ok(0);
            };
            let remaining = segment.len() - self.emitted;
            if remaining == 0 {
                self.segment += 1;
                self.emitted = 0;
                continue;
            }
            if buf.is_empty() {
                return Ok(0);
            }
            let n = usize::try_from(remaining.min(buf.len() as u64)).expect("bounded by buf len");
            match segment {
                Segment::BlobRange {
                    input_ix, offset, ..
                } => {
                    let source = &self.sources[*input_ix as usize];
                    source.read_at(offset + self.emitted, &mut buf[..n])?;
                }
                Segment::Fill { byte, .. } => buf[..n].fill(*byte),
                Segment::Literal { bytes } => {
                    let start = usize::try_from(self.emitted).expect("literal is capped");
                    buf[..n].copy_from_slice(&bytes[start..start + n]);
                }
            }
            self.emitted += n as u64;
            return Ok(n);
        }
    }
}

/// A resolved piece of an output range, in output order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece<'a> {
    Input {
        input_ix: u32,
        offset: u64,
        len: u64,
    },
    Fill {
        byte: u8,
        len: u64,
    },
    Literal(&'a [u8]),
}

/// Affine range translation: resolve `range` of the output into the minimal
/// list of pieces (adjacent input ranges and same-byte fills are merged).
/// This is what makes assemble-covered files servable by range without
/// materialization.
pub fn translate<'a>(
    params: &'a AssembleParams,
    range: Range<u64>,
) -> Result<Vec<Piece<'a>>, AssembleError> {
    if range.start > range.end || range.end > params.output_size()? {
        return Err(AssembleError::RangeOutOfBounds);
    }
    let mut pieces: Vec<Piece<'a>> = Vec::new();
    let mut segment_start = 0u64;
    for segment in &params.segments {
        let segment_end = segment_start + segment.len();
        let start = range.start.max(segment_start);
        let end = range.end.min(segment_end);
        if start < end {
            let within = start - segment_start;
            let len = end - start;
            push_merged(&mut pieces, segment, within, len);
        }
        segment_start = segment_end;
        if segment_start >= range.end {
            break;
        }
    }
    Ok(pieces)
}

fn push_merged<'a>(pieces: &mut Vec<Piece<'a>>, segment: &'a Segment, within: u64, len: u64) {
    let piece = match segment {
        Segment::BlobRange {
            input_ix, offset, ..
        } => Piece::Input {
            input_ix: *input_ix,
            offset: offset + within,
            len,
        },
        Segment::Fill { byte, .. } => Piece::Fill { byte: *byte, len },
        Segment::Literal { bytes } => {
            let start = usize::try_from(within).expect("literal is capped");
            let end = start + usize::try_from(len).expect("literal is capped");
            Piece::Literal(&bytes[start..end])
        }
    };
    match (pieces.last_mut(), &piece) {
        (
            Some(Piece::Input {
                input_ix: a,
                offset: a_off,
                len: a_len,
            }),
            Piece::Input {
                input_ix: b,
                offset: b_off,
                len: b_len,
            },
        ) if a == b && *a_off + *a_len == *b_off => *a_len += b_len,
        (
            Some(Piece::Fill {
                byte: a,
                len: a_len,
            }),
            Piece::Fill {
                byte: b,
                len: b_len,
            },
        ) if a == b => {
            *a_len += b_len;
        }
        _ => pieces.push(piece),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn ines_params() -> AssembleParams {
        // Worked example from docs/recipes.md: headered = header + body.
        AssembleParams {
            segments: vec![
                Segment::BlobRange {
                    input_ix: 0,
                    offset: 0,
                    len: 16,
                },
                Segment::BlobRange {
                    input_ix: 1,
                    offset: 0,
                    len: 8,
                },
                Segment::Fill { byte: 0xff, len: 4 },
                Segment::Literal {
                    bytes: b"tail".to_vec(),
                },
            ],
        }
    }

    fn materialize<S: Source>(params: &AssembleParams, sources: &[S]) -> Vec<u8> {
        let mut out = Vec::new();
        reader(params, sources)
            .expect("valid")
            .read_to_end(&mut out)
            .expect("reads");
        out
    }

    #[test]
    fn params_round_trip() {
        let params = ines_params();
        let encoded = params.encode().expect("valid");
        assert_eq!(AssembleParams::decode(&encoded).expect("decodes"), params);
    }

    #[test]
    fn rejects_bad_params() {
        let empty = AssembleParams { segments: vec![] };
        assert_eq!(
            empty.encode(),
            Err(AssembleError::Params("empty segment list"))
        );

        let zero = AssembleParams {
            segments: vec![Segment::Fill { byte: 0, len: 0 }],
        };
        assert_eq!(
            zero.encode(),
            Err(AssembleError::Params("zero-length segment"))
        );

        let big = AssembleParams {
            segments: vec![Segment::Literal {
                bytes: vec![0; LITERAL_CAP + 1],
            }],
        };
        assert_eq!(
            big.encode(),
            Err(AssembleError::Params("literal exceeds 4096-byte cap"))
        );

        // Mixed key set {1,2,3,4} is no variant.
        let bytes = cbor::encode(&Value::Array(vec![Value::Map(vec![
            (1, Value::Uint(0)),
            (2, Value::Uint(0)),
            (3, Value::Uint(1)),
            (4, Value::Uint(0)),
        ])]))
        .expect("encodable");
        assert_eq!(
            AssembleParams::decode(&bytes),
            Err(AssembleError::Params("unrecognized segment key set"))
        );
    }

    #[test]
    fn executes_and_validates_sources() {
        let header: &[u8] = &[0xaa; 16];
        let body: &[u8] = b"01234567";
        let sources = [header, body];
        let out = materialize(&ines_params(), &sources);
        assert_eq!(out.len(), 32);
        assert_eq!(&out[..16], header);
        assert_eq!(&out[16..24], body);
        assert_eq!(&out[24..28], &[0xff; 4]);
        assert_eq!(&out[28..], b"tail");

        let short: &[u8] = &[0; 4];
        let err = reader(&ines_params(), &[short, body])
            .err()
            .expect("header too short");
        assert_eq!(
            err,
            AssembleError::SourceRange {
                segment: 0,
                offset: 0,
                len: 16,
                source_len: 4
            }
        );
        let err = reader(&ines_params(), &[header])
            .err()
            .expect("missing input");
        assert_eq!(
            err,
            AssembleError::InputIndex {
                segment: 1,
                input_ix: 1
            }
        );
    }

    #[test]
    fn translate_merges_adjacent_pieces() {
        // Two contiguous ranges of the same input merge into one piece.
        let params = AssembleParams {
            segments: vec![
                Segment::BlobRange {
                    input_ix: 0,
                    offset: 0,
                    len: 4,
                },
                Segment::BlobRange {
                    input_ix: 0,
                    offset: 4,
                    len: 4,
                },
                Segment::Fill { byte: 0, len: 2 },
                Segment::Fill { byte: 0, len: 2 },
            ],
        };
        assert_eq!(
            translate(&params, 0..12).expect("in range"),
            vec![
                Piece::Input {
                    input_ix: 0,
                    offset: 0,
                    len: 8
                },
                Piece::Fill { byte: 0, len: 4 },
            ]
        );
        assert_eq!(translate(&params, 3..3).expect("empty ok"), vec![]);
        assert_eq!(
            translate(&params, 0..13),
            Err(AssembleError::RangeOutOfBounds)
        );
    }

    /// Segments valid for two sources of fixed lengths 64 and 16.
    fn segment_strategy() -> impl Strategy<Value = Segment> {
        prop_oneof![
            (0u64..64, 1u64..16).prop_map(|(offset, len)| Segment::BlobRange {
                input_ix: 0,
                offset: offset.min(63),
                len: len.min(64 - offset.min(63)).max(1),
            }),
            (0u64..16, 1u64..8).prop_map(|(offset, len)| Segment::BlobRange {
                input_ix: 1,
                offset: offset.min(15),
                len: len.min(16 - offset.min(15)).max(1),
            }),
            (any::<u8>(), 1u64..32).prop_map(|(byte, len)| Segment::Fill { byte, len }),
            prop::collection::vec(any::<u8>(), 1..32).prop_map(|bytes| Segment::Literal { bytes }),
        ]
    }

    proptest! {
        #[test]
        fn translate_equals_materialize_slice(
            segments in prop::collection::vec(segment_strategy(), 1..12),
            split in any::<(u64, u64)>(),
        ) {
            let params = AssembleParams { segments };
            let a: Vec<u8> = (0..64u8).collect();
            let b: Vec<u8> = (100..116u8).collect();
            let sources = [a.as_slice(), b.as_slice()];
            let full = materialize(&params, &sources);
            let size = params.output_size().expect("no overflow");
            prop_assert_eq!(full.len() as u64, size);

            let (x, y) = (split.0 % (size + 1), split.1 % (size + 1));
            let range = x.min(y)..x.max(y);
            let mut from_pieces = Vec::new();
            for piece in translate(&params, range.clone()).expect("in range") {
                match piece {
                    Piece::Input { input_ix, offset, len } => {
                        let mut buf = vec![0; usize::try_from(len).expect("small")];
                        sources[input_ix as usize].read_at(offset, &mut buf).expect("reads");
                        from_pieces.extend_from_slice(&buf);
                    }
                    Piece::Fill { byte, len } => {
                        from_pieces.extend(std::iter::repeat_n(byte, usize::try_from(len).expect("small")));
                    }
                    Piece::Literal(bytes) => from_pieces.extend_from_slice(bytes),
                }
            }
            let start = usize::try_from(range.start).expect("small");
            let end = usize::try_from(range.end).expect("small");
            prop_assert_eq!(from_pieces, full[start..end].to_vec());
        }

        #[test]
        fn params_round_trip_property(segments in prop::collection::vec(segment_strategy(), 1..12)) {
            let params = AssembleParams { segments };
            let encoded = params.encode().expect("valid");
            prop_assert_eq!(AssembleParams::decode(&encoded).expect("decodes"), params);
        }
    }
}
