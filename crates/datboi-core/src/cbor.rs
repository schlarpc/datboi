//! Strict canonical CBOR subset codec (RFC 8949 §4.2.1 deterministic
//! encoding). Recipe objects freeze these bytes forever (docs/recipes.md),
//! so the codec is self-owned and supports exactly the subset they need:
//! unsigned integers, byte strings, text strings, arrays, and maps with
//! unsigned-integer keys. Everything else — negative integers, floats, tags,
//! simple values, indefinite lengths — is inexpressible on encode and
//! rejected on decode.

const MAJOR_UINT: u8 = 0;
const MAJOR_NINT: u8 = 1;
const MAJOR_BYTES: u8 = 2;
const MAJOR_TEXT: u8 = 3;
const MAJOR_ARRAY: u8 = 4;
const MAJOR_MAP: u8 = 5;
const MAJOR_TAG: u8 = 6;

/// Decoder recursion guard; recipe objects are shallow, this is a resource
/// bound against hostile input, not a format limit.
pub const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Uint(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    /// Encoded sorted by key regardless of entry order here; duplicate keys
    /// are an encode error. Decoded maps are always sorted (decoder rejects
    /// anything else).
    Map(Vec<(u64, Value)>),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EncodeError {
    #[error("duplicate map key {0}")]
    DuplicateKey(u64),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("unexpected end of input at byte {0}")]
    UnexpectedEof(usize),
    #[error("{what} at byte {offset} is outside the canonical subset")]
    Forbidden { offset: usize, what: &'static str },
    #[error("non-minimal integer head at byte {0}")]
    NonMinimal(usize),
    #[error("unsorted or duplicate map key at byte {0}")]
    KeyOrder(usize),
    #[error("length at byte {0} exceeds remaining input")]
    Length(usize),
    #[error("invalid utf-8 in text string at byte {0}")]
    Utf8(usize),
    #[error("nesting deeper than {MAX_DEPTH} at byte {0}")]
    Depth(usize),
    #[error("trailing bytes at byte {0}")]
    Trailing(usize),
}

/// Encode to canonical bytes. Map entries are sorted here; the only failure
/// is a duplicate map key.
pub fn encode(value: &Value) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    encode_into(value, &mut out)?;
    Ok(out)
}

fn encode_into(value: &Value, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    match value {
        Value::Uint(n) => put_head(out, MAJOR_UINT, *n),
        Value::Bytes(b) => {
            put_head(out, MAJOR_BYTES, b.len() as u64);
            out.extend_from_slice(b);
        }
        Value::Text(t) => {
            put_head(out, MAJOR_TEXT, t.len() as u64);
            out.extend_from_slice(t.as_bytes());
        }
        Value::Array(items) => {
            put_head(out, MAJOR_ARRAY, items.len() as u64);
            for item in items {
                encode_into(item, out)?;
            }
        }
        Value::Map(entries) => {
            let mut sorted: Vec<&(u64, Value)> = entries.iter().collect();
            sorted.sort_by_key(|entry| entry.0);
            for pair in sorted.windows(2) {
                if pair[0].0 == pair[1].0 {
                    return Err(EncodeError::DuplicateKey(pair[0].0));
                }
            }
            put_head(out, MAJOR_MAP, sorted.len() as u64);
            for (key, val) in sorted {
                put_head(out, MAJOR_UINT, *key);
                encode_into(val, out)?;
            }
        }
    }
    Ok(())
}

/// Minimal-length head per §4.2.1: shortest additional-info form that fits.
fn put_head(out: &mut Vec<u8>, major: u8, value: u64) {
    let m = major << 5;
    if value < 24 {
        out.push(m | u8::try_from(value).expect("< 24"));
    } else if let Ok(v) = u8::try_from(value) {
        out.push(m | 24);
        out.push(v);
    } else if let Ok(v) = u16::try_from(value) {
        out.push(m | 25);
        out.extend_from_slice(&v.to_be_bytes());
    } else if let Ok(v) = u32::try_from(value) {
        out.push(m | 26);
        out.extend_from_slice(&v.to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

/// Decode exactly one canonical value spanning the whole input.
pub fn decode(input: &[u8]) -> Result<Value, DecodeError> {
    let mut d = Decoder { input, pos: 0 };
    let value = d.value(0)?;
    if d.pos != input.len() {
        return Err(DecodeError::Trailing(d.pos));
    }
    Ok(value)
}

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
}

impl Decoder<'_> {
    fn byte(&mut self) -> Result<u8, DecodeError> {
        let b = *self
            .input
            .get(self.pos)
            .ok_or(DecodeError::UnexpectedEof(self.pos))?;
        self.pos += 1;
        Ok(b)
    }

    fn be_uint(&mut self, width: usize) -> Result<u64, DecodeError> {
        let mut v: u64 = 0;
        for _ in 0..width {
            v = (v << 8) | u64::from(self.byte()?);
        }
        Ok(v)
    }

    /// Read a head, enforcing minimal-length encoding and definite lengths.
    fn head(&mut self) -> Result<(u8, u64), DecodeError> {
        let offset = self.pos;
        let initial = self.byte()?;
        let major = initial >> 5;
        let info = initial & 0x1f;
        let arg = match info {
            0..=23 => u64::from(info),
            24 => {
                let v = self.be_uint(1)?;
                if v < 24 {
                    return Err(DecodeError::NonMinimal(offset));
                }
                v
            }
            25 => {
                let v = self.be_uint(2)?;
                if v < 0x100 {
                    return Err(DecodeError::NonMinimal(offset));
                }
                v
            }
            26 => {
                let v = self.be_uint(4)?;
                if v < 0x1_0000 {
                    return Err(DecodeError::NonMinimal(offset));
                }
                v
            }
            27 => {
                let v = self.be_uint(8)?;
                if v < 0x1_0000_0000 {
                    return Err(DecodeError::NonMinimal(offset));
                }
                v
            }
            28..=30 => {
                return Err(DecodeError::Forbidden {
                    offset,
                    what: "reserved additional-info",
                });
            }
            _ => {
                return Err(DecodeError::Forbidden {
                    offset,
                    what: "indefinite length",
                });
            }
        };
        Ok((major, arg))
    }

    fn take(&mut self, len: u64, at: usize) -> Result<&[u8], DecodeError> {
        let len = usize::try_from(len).map_err(|_| DecodeError::Length(at))?;
        let end = self
            .pos
            .checked_add(len)
            .filter(|&e| e <= self.input.len())
            .ok_or(DecodeError::Length(at))?;
        let slice = &self.input[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn value(&mut self, depth: usize) -> Result<Value, DecodeError> {
        if depth > MAX_DEPTH {
            return Err(DecodeError::Depth(self.pos));
        }
        let offset = self.pos;
        let (major, arg) = self.head()?;
        match major {
            MAJOR_UINT => Ok(Value::Uint(arg)),
            MAJOR_NINT => Err(DecodeError::Forbidden {
                offset,
                what: "negative integer",
            }),
            MAJOR_BYTES => Ok(Value::Bytes(self.take(arg, offset)?.to_vec())),
            MAJOR_TEXT => {
                let bytes = self.take(arg, offset)?;
                let text = std::str::from_utf8(bytes).map_err(|_| DecodeError::Utf8(offset))?;
                Ok(Value::Text(text.to_owned()))
            }
            MAJOR_ARRAY => {
                // Every element occupies at least one byte.
                if arg > (self.input.len() - self.pos) as u64 {
                    return Err(DecodeError::Length(offset));
                }
                let mut items = Vec::new();
                for _ in 0..arg {
                    items.push(self.value(depth + 1)?);
                }
                Ok(Value::Array(items))
            }
            MAJOR_MAP => {
                if arg > (self.input.len() - self.pos) as u64 {
                    return Err(DecodeError::Length(offset));
                }
                let mut entries: Vec<(u64, Value)> = Vec::new();
                let mut prev: Option<u64> = None;
                for _ in 0..arg {
                    let key_offset = self.pos;
                    let (key_major, key) = self.head()?;
                    if key_major != MAJOR_UINT {
                        return Err(DecodeError::Forbidden {
                            offset: key_offset,
                            what: "non-integer map key",
                        });
                    }
                    if prev.is_some_and(|p| p >= key) {
                        return Err(DecodeError::KeyOrder(key_offset));
                    }
                    prev = Some(key);
                    entries.push((key, self.value(depth + 1)?));
                }
                Ok(Value::Map(entries))
            }
            MAJOR_TAG => Err(DecodeError::Forbidden {
                offset,
                what: "tag",
            }),
            _ => Err(DecodeError::Forbidden {
                offset,
                what: "float or simple value",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn golden_encodings() {
        let cases: &[(Value, &[u8])] = &[
            (Value::Uint(0), &[0x00]),
            (Value::Uint(23), &[0x17]),
            (Value::Uint(24), &[0x18, 24]),
            (Value::Uint(500), &[0x19, 0x01, 0xf4]),
            (
                Value::Uint(u64::MAX),
                &[0x1b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            ),
            (Value::Bytes(vec![]), &[0x40]),
            (Value::Text("a".into()), &[0x61, 0x61]),
            (
                // Entry order does not matter; encoding sorts.
                Value::Map(vec![(1, Value::Uint(2)), (0, Value::Uint(1))]),
                &[0xa2, 0x00, 0x01, 0x01, 0x02],
            ),
            (
                Value::Array(vec![Value::Uint(1), Value::Text("x".into())]),
                &[0x82, 0x01, 0x61, 0x78],
            ),
        ];
        for (value, expected) in cases {
            let encoded = encode(value).expect("encodable");
            assert_eq!(&encoded, expected, "{value:?}");
            let mut normalized = value.clone();
            if let Value::Map(entries) = &mut normalized {
                entries.sort_by_key(|entry| entry.0);
            }
            assert_eq!(decode(&encoded).expect("decodable"), normalized);
        }
    }

    #[test]
    fn duplicate_keys_rejected_on_encode() {
        let v = Value::Map(vec![(7, Value::Uint(0)), (7, Value::Uint(1))]);
        assert_eq!(encode(&v), Err(EncodeError::DuplicateKey(7)));
    }

    #[test]
    fn known_bad_vectors() {
        let cases: &[(&[u8], DecodeError)] = &[
            // Non-minimal heads.
            (&[0x18, 0x17], DecodeError::NonMinimal(0)),
            (&[0x19, 0x00, 0xff], DecodeError::NonMinimal(0)),
            (&[0x1a, 0x00, 0x00, 0xff, 0xff], DecodeError::NonMinimal(0)),
            (
                &[0x1b, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff],
                DecodeError::NonMinimal(0),
            ),
            // Outside the subset.
            (
                &[0x20],
                DecodeError::Forbidden {
                    offset: 0,
                    what: "negative integer",
                },
            ),
            (
                &[0xc0, 0x00],
                DecodeError::Forbidden {
                    offset: 0,
                    what: "tag",
                },
            ),
            (
                &[0xf6],
                DecodeError::Forbidden {
                    offset: 0,
                    what: "float or simple value",
                },
            ),
            (
                &[0xfa, 0x3f, 0x80, 0x00, 0x00],
                DecodeError::Forbidden {
                    offset: 0,
                    what: "float or simple value",
                },
            ),
            (
                &[0x5f, 0xff],
                DecodeError::Forbidden {
                    offset: 0,
                    what: "indefinite length",
                },
            ),
            (
                &[0x9f],
                DecodeError::Forbidden {
                    offset: 0,
                    what: "indefinite length",
                },
            ),
            (
                &[0x1c],
                DecodeError::Forbidden {
                    offset: 0,
                    what: "reserved additional-info",
                },
            ),
            (
                &[0xa1, 0x61, 0x61, 0x00],
                DecodeError::Forbidden {
                    offset: 1,
                    what: "non-integer map key",
                },
            ),
            // Map key ordering.
            (&[0xa2, 0x01, 0x00, 0x01, 0x00], DecodeError::KeyOrder(3)),
            (&[0xa2, 0x02, 0x00, 0x01, 0x00], DecodeError::KeyOrder(3)),
            // Framing.
            (&[0x00, 0x00], DecodeError::Trailing(1)),
            (&[0x41], DecodeError::Length(0)),
            (&[0x18], DecodeError::UnexpectedEof(1)),
            (&[], DecodeError::UnexpectedEof(0)),
            (&[0x62, 0xff, 0xff], DecodeError::Utf8(0)),
            (&[0x81], DecodeError::Length(0)),
        ];
        for (bytes, expected) in cases {
            assert_eq!(
                &decode(bytes).expect_err("must reject"),
                expected,
                "{bytes:02x?}"
            );
        }
    }

    #[test]
    fn depth_guard() {
        let mut bytes = vec![0x81; MAX_DEPTH + 1];
        bytes.push(0x00);
        assert!(matches!(decode(&bytes), Err(DecodeError::Depth(_))));
    }

    fn value_strategy() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            any::<u64>().prop_map(Value::Uint),
            prop::collection::vec(any::<u8>(), 0..32).prop_map(Value::Bytes),
            prop::collection::vec(any::<char>(), 0..16)
                .prop_map(|chars| Value::Text(chars.into_iter().collect())),
        ];
        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
                prop::collection::btree_map(any::<u64>(), inner, 0..8)
                    .prop_map(|m| Value::Map(m.into_iter().collect())),
            ]
        })
    }

    proptest! {
        #[test]
        fn round_trip_is_byte_identical(value in value_strategy()) {
            let first = encode(&value).expect("encodable");
            let decoded = decode(&first).expect("canonical bytes decode");
            let second = encode(&decoded).expect("re-encodable");
            prop_assert_eq!(first, second);
        }
    }
}
