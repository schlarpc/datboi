//! Per-op param schemas whose encode and decode live together
//! (docs/recipes.md: the op owns its params schema). The CBOR key
//! numbers exist HERE and nowhere else — ingest encodes at mint, exec
//! decodes at replay, so a renumber on one side can no longer poison
//! valid recipes as malformed. (`assemble@1` keeps its schema in
//! [`crate::assemble`], same discipline.)

use crate::cbor::{self, Value};

/// CBOR key for the extractor member index (D58).
const EXTRACTOR_KEY_MEMBER_IX: u64 = 1;

/// `deflate-decompress@1` window keys: `{1: offset, 2: len}`.
const DEFLATE_KEY_OFFSET: u64 = 1;
const DEFLATE_KEY_LEN: u64 = 2;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParamsError {
    #[error(transparent)]
    Cbor(#[from] cbor::DecodeError),
    #[error("{0}")]
    Invalid(&'static str),
}

/// `datboi:extractor@1` params (D58): which member of the container
/// (input 0) the single output is, as numbered by `enumerate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractorParams {
    pub member_ix: u32,
}

impl ExtractorParams {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        cbor::encode(&Value::Map(vec![(
            EXTRACTOR_KEY_MEMBER_IX,
            Value::Uint(u64::from(self.member_ix)),
        )]))
        .expect("one fixed key is canonical")
    }

    /// # Errors
    /// If the bytes are not exactly the one-field canonical map.
    pub fn decode(params: &[u8]) -> Result<Self, ParamsError> {
        let Value::Map(entries) = cbor::decode(params)? else {
            return Err(ParamsError::Invalid("extractor params must be a map"));
        };
        if entries.len() != 1 {
            return Err(ParamsError::Invalid(
                "extractor params: exactly one field (member index)",
            ));
        }
        match entries.iter().find(|(k, _)| *k == EXTRACTOR_KEY_MEMBER_IX) {
            Some((_, Value::Uint(n))) => Ok(Self {
                member_ix: u32::try_from(*n)
                    .map_err(|_| ParamsError::Invalid("member index out of range"))?,
            }),
            _ => Err(ParamsError::Invalid("extractor member index missing")),
        }
    }
}

/// `deflate-decompress@1` params: a window into input 0 — one recipe
/// per zip member instead of a slice-recipe + intermediate blob per
/// member (docs/recipes.md window amendment; at MAME scale the row
/// economy matters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeflateWindow {
    pub offset: u64,
    pub len: u64,
}

impl DeflateWindow {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        cbor::encode(&Value::Map(vec![
            (DEFLATE_KEY_OFFSET, Value::Uint(self.offset)),
            (DEFLATE_KEY_LEN, Value::Uint(self.len)),
        ]))
        .expect("two fixed keys are canonical")
    }

    /// # Errors
    /// If the bytes are not exactly the two-field canonical map.
    pub fn decode(params: &[u8]) -> Result<Self, ParamsError> {
        let Value::Map(entries) = cbor::decode(params)? else {
            return Err(ParamsError::Invalid("deflate params must be a map"));
        };
        if entries.len() != 2 {
            return Err(ParamsError::Invalid("deflate params: extra fields"));
        }
        let field = |key: u64| match entries.iter().find(|(k, _)| *k == key) {
            Some((_, Value::Uint(n))) => Ok(*n),
            _ => Err(ParamsError::Invalid("deflate window field missing")),
        };
        Ok(Self {
            offset: field(DEFLATE_KEY_OFFSET)?,
            len: field(DEFLATE_KEY_LEN)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FORMAT COMMITMENT: these bytes are recipe identity inputs. If
    /// this test fails, every existing extractor/deflate recipe hash
    /// changes — never acceptable after the M1 freeze (D5).
    #[test]
    fn wire_bytes_are_pinned() {
        assert_eq!(
            ExtractorParams { member_ix: 3 }.encode(),
            [0xa1, 0x01, 0x03]
        );
        assert_eq!(
            DeflateWindow {
                offset: 17,
                len: 300
            }
            .encode(),
            [0xa2, 0x01, 0x11, 0x02, 0x19, 0x01, 0x2c]
        );
    }

    #[test]
    fn round_trips() {
        for member_ix in [0, 1, u32::MAX] {
            let p = ExtractorParams { member_ix };
            assert_eq!(ExtractorParams::decode(&p.encode()), Ok(p));
        }
        for (offset, len) in [(0, 0), (17, 300), (u64::MAX, u64::MAX)] {
            let w = DeflateWindow { offset, len };
            assert_eq!(DeflateWindow::decode(&w.encode()), Ok(w));
        }
    }

    #[test]
    fn rejects_malformed() {
        // Not a map / wrong arity / wrong keys / out-of-range index.
        assert!(ExtractorParams::decode(&[0x80]).is_err());
        assert!(ExtractorParams::decode(&DeflateWindow { offset: 1, len: 2 }.encode()).is_err());
        let wrong_key = cbor::encode(&Value::Map(vec![(9, Value::Uint(0))])).expect("cbor");
        assert!(ExtractorParams::decode(&wrong_key).is_err());
        let too_big = cbor::encode(&Value::Map(vec![(1, Value::Uint(u64::MAX))])).expect("cbor");
        assert!(ExtractorParams::decode(&too_big).is_err());
        assert!(DeflateWindow::decode(&[0x80]).is_err());
        assert!(DeflateWindow::decode(&ExtractorParams { member_ix: 0 }.encode()).is_err());
    }
}
