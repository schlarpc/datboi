//! `xf-wii-crypt`: the `encrypt` and `decrypt` ops for the
//! `datboi:transform@1` world (D116).
//!
//! Both ops take two inputs — the partition body in one direction and
//! the COMMON key (16 bytes, D12: a key is an ordinary blob input) —
//! and the same params (the wrapped title key, the title ID, the
//! sector count). `encrypt` turns `sectors × 0x7C00` bytes of plaintext
//! into `sectors × 0x8000` bytes of ciphertext, recomputing every hash
//! block from the data; `decrypt` is the inverse and discards the hash
//! blocks.
//!
//! `serve-range` works at the format's natural quanta: a 2 MiB hash
//! group for `encrypt` (a sector's ciphertext needs its group's H2s),
//! one 32 KiB sector for `decrypt` (each sector carries its own IV). A
//! window costs the groups it touches and nothing before them, so an
//! evicted disc's assemble serves ranges through this node in place
//! (D111's seekable-child path) instead of spilling the partition.

use datboi_guest_transform::{Descriptor, File, Guest, Input, SeekClass, Sink, Source};

use crate::params::Params;
use crate::{
    DATA_LEN, GROUP_DATA_LEN, GROUP_SECTORS, GroupHashes, Key, SECTOR_LEN, decrypt_data,
    encrypt_sector, title_key,
};

const OP_ENCRYPT: &str = "encrypt";
const OP_DECRYPT: &str = "decrypt";

struct Xf;

fn params(op: &str, params: &[u8]) -> Result<Params, String> {
    if op != OP_ENCRYPT && op != OP_DECRYPT {
        return Err(format!("unknown op {op:?}"));
    }
    Params::decode(params).map_err(String::from)
}

fn expect_sequential<'a>(input: &'a Input, what: &str) -> Result<&'a Source, String> {
    match input {
        Input::Sequential(s) => Ok(s),
        Input::RandomAccess(_) => Err(format!("{what}: run reads its inputs sequentially")),
    }
}

fn expect_random<'a>(input: &'a Input, what: &str) -> Result<&'a File, String> {
    match input {
        Input::RandomAccess(f) => Ok(f),
        Input::Sequential(_) => Err(format!("{what}: serve-range inputs must be random-access")),
    }
}

fn read_exact_seq(src: &Source, n: usize, what: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let want = u32::try_from(n - out.len()).map_err(|_| format!("{what}: huge read"))?;
        let piece = src.read(want);
        if piece.is_empty() {
            return Err(format!(
                "{what}: wanted {n} bytes, stream ended after {}",
                out.len()
            ));
        }
        out.extend_from_slice(&piece);
    }
    Ok(out)
}

fn read_exact_at(file: &File, offset: u64, n: usize, what: &str) -> Result<Vec<u8>, String> {
    let bytes = file.read_at(
        offset,
        u32::try_from(n).map_err(|_| format!("{what}: huge read"))?,
    );
    if bytes.len() != n {
        return Err(format!(
            "{what}: wanted {n} bytes at {offset}, got {}",
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// The common key input: exactly 16 bytes, whichever shape it arrived as.
fn common_key(input: &Input) -> Result<Key, String> {
    let bytes = match input {
        Input::Sequential(s) => {
            if s.len() != 16 {
                return Err(format!("common key must be 16 bytes, got {}", s.len()));
            }
            read_exact_seq(s, 16, "common key")?
        }
        Input::RandomAccess(f) => {
            if f.len() != 16 {
                return Err(format!("common key must be 16 bytes, got {}", f.len()));
            }
            read_exact_at(f, 0, 16, "common key")?
        }
    };
    let mut key = [0u8; 16];
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// Two inputs, in recipe order: the body, then the key.
fn two<'a>(inputs: &'a [Input], op: &str) -> Result<(&'a Input, &'a Input), String> {
    match inputs {
        [body, key] => Ok((body, key)),
        _ => Err(format!(
            "{op} takes 2 inputs (body, common key), got {}",
            inputs.len()
        )),
    }
}

fn one_output<'a>(outputs: &'a [Sink], op: &str) -> Result<&'a Sink, String> {
    match outputs {
        [out] => Ok(out),
        _ => Err(format!("{op} claims 1 output, got {}", outputs.len())),
    }
}

/// Sectors in the group `g` of a partition of `sectors` sectors.
fn group_sectors(p: &Params, g: u64) -> usize {
    usize::try_from((p.sectors - g * GROUP_SECTORS as u64).min(GROUP_SECTORS as u64))
        .expect("at most 64")
}

impl Guest for Xf {
    fn describe(op: String) -> Result<Vec<u8>, String> {
        if op != OP_ENCRYPT && op != OP_DECRYPT {
            return Err(format!("unknown op {op:?}"));
        }
        // Ranges map arithmetically at a fixed quantum (a 2 MiB hash
        // group encrypting, a 32 KiB sector decrypting); `run` reads
        // both inputs sequentially.
        Ok(Descriptor {
            seek: SeekClass::Affine,
            random_access_inputs: Vec::new(),
        }
        .to_cbor())
    }

    fn run(
        op: String,
        params_bytes: Vec<u8>,
        inputs: Vec<Input>,
        outputs: Vec<Sink>,
    ) -> Result<(), String> {
        let p = params(&op, &params_bytes)?;
        let (body, key_input) = two(&inputs, &op)?;
        let out = one_output(&outputs, &op)?;
        let key = title_key(&common_key(key_input)?, &p.wrapped_title_key, &p.title_id);
        let body = expect_sequential(body, &op)?;
        if op == OP_ENCRYPT {
            if body.len() != p.plain_len() {
                return Err(format!(
                    "plaintext is {} bytes, params describe {}",
                    body.len(),
                    p.plain_len()
                ));
            }
            let groups = p.sectors.div_ceil(GROUP_SECTORS as u64);
            let mut sector_out = [0u8; SECTOR_LEN];
            for g in 0..groups {
                let n = group_sectors(&p, g);
                let data = read_exact_seq(body, n * DATA_LEN, "plaintext group")?;
                let hashes = GroupHashes::compute(&data);
                for s in 0..n {
                    encrypt_sector(
                        &key,
                        &hashes,
                        s,
                        &data[s * DATA_LEN..(s + 1) * DATA_LEN],
                        &mut sector_out,
                    );
                    out.write(&sector_out);
                }
            }
        } else {
            if body.len() != p.encrypted_len() {
                return Err(format!(
                    "ciphertext is {} bytes, params describe {}",
                    body.len(),
                    p.encrypted_len()
                ));
            }
            let mut data = [0u8; DATA_LEN];
            for _ in 0..p.sectors {
                let enc = read_exact_seq(body, SECTOR_LEN, "encrypted sector")?;
                let enc: &[u8; SECTOR_LEN] = enc.as_slice().try_into().expect("exact");
                decrypt_data(&key, enc, &mut data);
                out.write(&data);
            }
        }
        Ok(())
    }

    fn serve_range(
        op: String,
        params_bytes: Vec<u8>,
        inputs: Vec<Input>,
        output_ix: u32,
        offset: u64,
        len: u64,
        out: Sink,
    ) -> Result<(), String> {
        let p = params(&op, &params_bytes)?;
        let (body, key_input) = two(&inputs, &op)?;
        if output_ix != 0 {
            return Err(format!("no output {output_ix}: {op} is 1-output"));
        }
        let key = title_key(&common_key(key_input)?, &p.wrapped_title_key, &p.title_id);
        let body = expect_random(body, &op)?;
        if op == OP_ENCRYPT {
            let total = p.encrypted_len();
            let start = offset.min(total);
            let end = offset.saturating_add(len).min(total);
            if end <= start {
                return Ok(());
            }
            let first_sector = start / SECTOR_LEN as u64;
            let last_sector = (end - 1) / SECTOR_LEN as u64;
            let mut sector_out = [0u8; SECTOR_LEN];
            for g in first_sector / GROUP_SECTORS as u64..=last_sector / GROUP_SECTORS as u64 {
                let n = group_sectors(&p, g);
                let data = read_exact_at(
                    body,
                    g * GROUP_DATA_LEN as u64,
                    n * DATA_LEN,
                    "plaintext group",
                )?;
                let hashes = GroupHashes::compute(&data);
                let s0 = first_sector.max(g * GROUP_SECTORS as u64);
                let s1 = last_sector.min(g * GROUP_SECTORS as u64 + n as u64 - 1);
                for sector in s0..=s1 {
                    let s = usize::try_from(sector - g * GROUP_SECTORS as u64).expect("< 64");
                    encrypt_sector(
                        &key,
                        &hashes,
                        s,
                        &data[s * DATA_LEN..(s + 1) * DATA_LEN],
                        &mut sector_out,
                    );
                    let base = sector * SECTOR_LEN as u64;
                    let lo = usize::try_from(start.max(base) - base).expect("< sector");
                    let hi = usize::try_from(end.min(base + SECTOR_LEN as u64) - base)
                        .expect("<= sector");
                    out.write(&sector_out[lo..hi]);
                }
            }
        } else {
            let total = p.plain_len();
            let start = offset.min(total);
            let end = offset.saturating_add(len).min(total);
            if end <= start {
                return Ok(());
            }
            let mut data = [0u8; DATA_LEN];
            for sector in start / DATA_LEN as u64..=(end - 1) / DATA_LEN as u64 {
                let enc = read_exact_at(
                    body,
                    sector * SECTOR_LEN as u64,
                    SECTOR_LEN,
                    "encrypted sector",
                )?;
                let enc: &[u8; SECTOR_LEN] = enc.as_slice().try_into().expect("exact");
                decrypt_data(&key, enc, &mut data);
                let base = sector * DATA_LEN as u64;
                let lo = usize::try_from(start.max(base) - base).expect("< sector");
                let hi =
                    usize::try_from(end.min(base + DATA_LEN as u64) - base).expect("<= sector");
                out.write(&data[lo..hi]);
            }
        }
        Ok(())
    }
}

datboi_guest_transform::export!(Xf);
