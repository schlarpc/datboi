//! clrmamepro text format parser: paren-delimited sections
//! (`clrmamepro ( … )`, `game ( … rom ( name x size n crc c ) )`), still
//! emitted by DAT-o-MATIC profiles and old-style MAME dats.
//!
//! Tokenizer notes: values may be double-quoted (quoting is the only way
//! names containing spaces or parens survive — no escape sequences exist in
//! the wild); bare tokens end at whitespace or a paren. `resource` sections
//! are bios containers (old MAME convention) and import as `is_bios`
//! entries. Unknown scalar keys are preserved into attrs; unknown *blocks*
//! are preserved as their space-joined token stream (the CAS dat blob
//! remains the canonical form).

use crate::model::{
    ClaimKind, ClaimStatus, DatFile, DatHeader, Entry, ParseError, RomClaim, parse_crc32,
    parse_md5, parse_sha1, parse_sha256,
};

pub fn parse(bytes: &[u8]) -> Result<DatFile, ParseError> {
    let text = String::from_utf8_lossy(bytes);
    let tokens = tokenize(&text)?;
    let mut dat = DatFile::default();
    let mut cursor = 0usize;
    let mut saw_section = false;

    while cursor < tokens.len() {
        let (pos, section) = expect_word(&tokens, &mut cursor, "section name")?;
        let section = section.to_string();
        expect_open(&tokens, &mut cursor, &section)?;
        match section.as_str() {
            "clrmamepro" | "emulator" => {
                parse_header_section(&tokens, &mut cursor, &mut dat.header)?;
            }
            "game" | "machine" | "set" => {
                dat.entries
                    .push(parse_game_section(&tokens, &mut cursor, false)?);
            }
            "resource" => {
                dat.entries
                    .push(parse_game_section(&tokens, &mut cursor, true)?);
            }
            _ => {
                return Err(ParseError::invalid(
                    pos as u64,
                    format!("unknown section {section:?}"),
                ));
            }
        }
        saw_section = true;
    }
    if !saw_section {
        return Err(ParseError::invalid(0, "empty clrmamepro dat"));
    }
    Ok(dat)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Open,
    Close,
    Word(String),
}

/// One `key …` step inside a section.
enum Pair {
    Scalar(String, String),
    /// `key (` was consumed; the block body is next at the cursor.
    Block(String),
}

fn tokenize(text: &str) -> Result<Vec<(usize, Tok)>, ParseError> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            c if c.is_ascii_whitespace() => i += 1,
            b'(' => {
                out.push((i, Tok::Open));
                i += 1;
            }
            b')' => {
                out.push((i, Tok::Close));
                i += 1;
            }
            b'"' => {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += 1;
                }
                if j >= bytes.len() {
                    return Err(ParseError::invalid(i as u64, "unterminated quoted string"));
                }
                out.push((i, Tok::Word(text[start..j].to_string())));
                i = j + 1;
            }
            _ => {
                let start = i;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && !matches!(bytes[i], b'(' | b')' | b'"')
                {
                    i += 1;
                }
                out.push((start, Tok::Word(text[start..i].to_string())));
            }
        }
    }
    Ok(out)
}

fn expect_word<'t>(
    tokens: &'t [(usize, Tok)],
    cursor: &mut usize,
    what: &str,
) -> Result<(usize, &'t str), ParseError> {
    match tokens.get(*cursor) {
        Some((pos, Tok::Word(w))) => {
            *cursor += 1;
            Ok((*pos, w))
        }
        Some((pos, _)) => Err(ParseError::invalid(*pos as u64, format!("expected {what}"))),
        None => Err(ParseError::invalid(0, format!("expected {what}, got EOF"))),
    }
}

fn expect_open(
    tokens: &[(usize, Tok)],
    cursor: &mut usize,
    section: &str,
) -> Result<(), ParseError> {
    match tokens.get(*cursor) {
        Some((_, Tok::Open)) => {
            *cursor += 1;
            Ok(())
        }
        other => {
            let pos = other.map_or(0, |(p, _)| *p);
            Err(ParseError::invalid(
                pos as u64,
                format!("expected '(' after {section:?}"),
            ))
        }
    }
}

/// Step to the next `key value` / `key (` pair; `None` = section Close
/// consumed.
fn next_pair(tokens: &[(usize, Tok)], cursor: &mut usize) -> Result<Option<Pair>, ParseError> {
    match tokens.get(*cursor) {
        Some((_, Tok::Close)) => {
            *cursor += 1;
            Ok(None)
        }
        Some((_, Tok::Word(_))) => {
            let (pos, key) = expect_word(tokens, cursor, "key")?;
            let key = key.to_string();
            match tokens.get(*cursor) {
                Some((_, Tok::Open)) => {
                    *cursor += 1;
                    Ok(Some(Pair::Block(key)))
                }
                Some((_, Tok::Word(_))) => {
                    let (_, value) = expect_word(tokens, cursor, "value")?;
                    Ok(Some(Pair::Scalar(key, value.to_string())))
                }
                _ => Err(ParseError::invalid(
                    pos as u64,
                    format!("key {key:?} without value"),
                )),
            }
        }
        Some((pos, Tok::Open)) => Err(ParseError::invalid(*pos as u64, "unexpected '('")),
        None => Err(ParseError::invalid(0, "unterminated section")),
    }
}

/// Consume a balanced block, returning its space-joined token stream
/// (preservation for blocks we don't model).
fn consume_block_raw(tokens: &[(usize, Tok)], cursor: &mut usize) -> Result<String, ParseError> {
    let mut depth = 1usize;
    let mut words = Vec::new();
    while depth > 0 {
        match tokens.get(*cursor) {
            Some((_, Tok::Open)) => {
                depth += 1;
                words.push("(".to_string());
            }
            Some((_, Tok::Close)) => {
                depth -= 1;
                if depth > 0 {
                    words.push(")".to_string());
                }
            }
            Some((_, Tok::Word(w))) => words.push(w.clone()),
            None => return Err(ParseError::invalid(0, "unterminated block")),
        }
        *cursor += 1;
    }
    Ok(words.join(" "))
}

fn parse_header_section(
    tokens: &[(usize, Tok)],
    cursor: &mut usize,
    header: &mut DatHeader,
) -> Result<(), ParseError> {
    while let Some(pair) = next_pair(tokens, cursor)? {
        match pair {
            Pair::Scalar(key, value) => match key.as_str() {
                "name" => header.name = Some(value),
                "description" => header.description = Some(value),
                "version" => header.version = Some(value),
                "date" => header.date = Some(value),
                "author" => header.author = Some(value),
                "homepage" => header.homepage = Some(value),
                "url" => header.url = Some(value),
                "header" => header.detector = Some(value),
                "forcemerging" => header.force_merging = Some(value),
                "forcenodump" => header.force_nodump = Some(value),
                "forcepacking" => header.force_packing = Some(value),
                _ => {
                    header.attrs.insert(key, value);
                }
            },
            Pair::Block(key) => {
                let raw = consume_block_raw(tokens, cursor)?;
                header.attrs.insert(key, raw);
            }
        }
    }
    Ok(())
}

fn parse_game_section(
    tokens: &[(usize, Tok)],
    cursor: &mut usize,
    is_bios: bool,
) -> Result<Entry, ParseError> {
    let mut entry = Entry {
        is_bios,
        ..Entry::default()
    };
    while let Some(pair) = next_pair(tokens, cursor)? {
        match pair {
            Pair::Scalar(key, value) => match key.as_str() {
                "name" => entry.name = value,
                "description" => entry.description = Some(value),
                "year" => entry.year = Some(value),
                "manufacturer" => entry.manufacturer = Some(value),
                "cloneof" => entry.cloneof = Some(value),
                "romof" => entry.romof = Some(value),
                "sampleof" => entry.sampleof = Some(value),
                "sample" => entry.claims.push(RomClaim {
                    kind: ClaimKind::Sample,
                    name: value,
                    ..RomClaim::default()
                }),
                _ => {
                    entry.attrs.insert(key, value);
                }
            },
            Pair::Block(key) => match key.as_str() {
                "rom" => {
                    let claim = parse_claim_block(tokens, cursor, ClaimKind::Rom)?;
                    entry.claims.push(claim);
                }
                "disk" => {
                    let claim = parse_claim_block(tokens, cursor, ClaimKind::Disk)?;
                    entry.claims.push(claim);
                }
                _ => {
                    let raw = consume_block_raw(tokens, cursor)?;
                    entry.attrs.insert(key, raw);
                }
            },
        }
    }
    if entry.name.is_empty() {
        return Err(ParseError::invalid(0, "game section without name"));
    }
    Ok(entry)
}

fn parse_claim_block(
    tokens: &[(usize, Tok)],
    cursor: &mut usize,
    kind: ClaimKind,
) -> Result<RomClaim, ParseError> {
    let mut claim = RomClaim {
        kind,
        ..RomClaim::default()
    };
    while let Some(pair) = next_pair(tokens, cursor)? {
        match pair {
            Pair::Scalar(key, value) => match key.as_str() {
                "name" => claim.name = value,
                "size" => {
                    claim.size =
                        Some(value.parse().map_err(|_| {
                            ParseError::invalid(0, format!("bad rom size {value:?}"))
                        })?);
                }
                "crc" | "crc32" => claim.crc32 = require(parse_crc32(&value), "crc", &value)?,
                "md5" => claim.md5 = require(parse_md5(&value), "md5", &value)?,
                "sha1" => claim.sha1 = require(parse_sha1(&value), "sha1", &value)?,
                "sha256" => claim.sha256 = require(parse_sha256(&value), "sha256", &value)?,
                "merge" => claim.merge_name = Some(value),
                "flags" | "status" => {
                    claim.status = ClaimStatus::parse(&value).ok_or_else(|| {
                        ParseError::invalid(0, format!("unknown flags {value:?}"))
                    })?;
                }
                _ => {
                    claim.attrs.insert(key, value);
                }
            },
            Pair::Block(key) => {
                let raw = consume_block_raw(tokens, cursor)?;
                claim.attrs.insert(key, raw);
            }
        }
    }
    Ok(claim)
}

fn require<T>(parsed: Option<T>, what: &str, raw: &str) -> Result<Option<T>, ParseError> {
    if raw.is_empty() || raw == "-" {
        return Ok(None);
    }
    match parsed {
        Some(v) => Ok(Some(v)),
        None => Err(ParseError::invalid(
            0,
            format!("malformed {what} value {raw:?}"),
        )),
    }
}
