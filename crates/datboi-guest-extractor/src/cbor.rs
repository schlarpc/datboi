//! Minimal canonical-CBOR ENCODER (RFC 8949 §4.2.1 deterministic
//! subset) for guest-side vocabulary: descriptors here, member lists in
//! datboi-guest-extractor (a deliberate small copy — guest crates ship
//! standalone to crates.io, so they cannot reach datboi-core, and the
//! schemas these bytes carry are frozen; the runtime conformance gates
//! decode every guest encoding with the host codec, so drift cannot
//! hide). Encode-only: params DECODING stays op-owned in each guest
//! (the op owns its params schema, docs/recipes.md).

use alloc::string::String;
use alloc::vec::Vec;

/// The value subset the vocabulary schemas need: unsigned integers,
/// byte strings, text, arrays, uint-keyed maps. Everything else is
/// inexpressible, matching the house codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Uint(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    /// Entries are sorted by key on encode; duplicate keys panic (the
    /// builders in this crate use fixed distinct keys).
    Map(Vec<(u64, Value)>),
}

/// Encode to canonical bytes.
///
/// # Panics
/// On duplicate map keys — unreachable through this crate's builders.
#[must_use]
pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(value, &mut out);
    out
}

fn encode_into(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Uint(n) => put_head(out, 0, *n),
        Value::Bytes(b) => {
            put_head(out, 2, b.len() as u64);
            out.extend_from_slice(b);
        }
        Value::Text(t) => {
            put_head(out, 3, t.len() as u64);
            out.extend_from_slice(t.as_bytes());
        }
        Value::Array(items) => {
            put_head(out, 4, items.len() as u64);
            for item in items {
                encode_into(item, out);
            }
        }
        Value::Map(entries) => {
            let mut sorted: Vec<&(u64, Value)> = entries.iter().collect();
            sorted.sort_by_key(|(k, _)| *k);
            assert!(
                sorted.windows(2).all(|w| w[0].0 != w[1].0),
                "duplicate map key"
            );
            put_head(out, 5, sorted.len() as u64);
            for (key, item) in sorted {
                put_head(out, 0, *key);
                encode_into(item, out);
            }
        }
    }
}

/// Shortest-form head (canonical rule: minimal integer encoding).
fn put_head(out: &mut Vec<u8>, major: u8, n: u64) {
    let major = major << 5;
    match n {
        0..=23 => out.push(major | u8::try_from(n).expect("<= 23")),
        24..=0xff => {
            out.push(major | 24);
            out.push(u8::try_from(n).expect("<= 0xff"));
        }
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend_from_slice(&u16::try_from(n).expect("bounded").to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend_from_slice(&u32::try_from(n).expect("bounded").to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend_from_slice(&n.to_be_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heads_are_shortest_form() {
        assert_eq!(encode(&Value::Uint(0)), [0x00]);
        assert_eq!(encode(&Value::Uint(23)), [0x17]);
        assert_eq!(encode(&Value::Uint(24)), [0x18, 24]);
        assert_eq!(encode(&Value::Uint(256)), [0x19, 0x01, 0x00]);
        assert_eq!(encode(&Value::Uint(1 << 16)), [0x1a, 0, 1, 0, 0]);
        assert_eq!(
            encode(&Value::Uint(1 << 32)),
            [0x1b, 0, 0, 0, 1, 0, 0, 0, 0]
        );
    }

    #[test]
    fn maps_sort_and_nest() {
        let v = Value::Map(vec![
            (2, Value::Array(vec![Value::Uint(1)])),
            (1, Value::Text("a".into())),
        ]);
        assert_eq!(encode(&v), [0xa2, 0x01, 0x61, b'a', 0x02, 0x81, 0x01]);
    }
}
