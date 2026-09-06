//! Gate for the `xf-wii-crypt` component (D116): Wii partition bodies
//! re-encrypted (`encrypt`) and decrypted (`decrypt`) under wasmtime.
//! Expected bytes come from the SAME crate the component compiles from
//! (the twin the analyzer verifies with natively), so this is a
//! wasm-vs-native equivalence check on top of the pinned fixture,
//! determinism, and D49 seek-equivalence at both quanta — a 2 MiB hash
//! group encrypting, a 32 KiB sector decrypting.

use std::io::Write;
use std::sync::{Arc, LazyLock, Mutex};

use datboi_runtime::stream::{RangeRead, RangeRequest, StreamHost, StreamTransform};
use datboi_runtime::{Limits, RuntimeError, SeekClass};
use datboi_xf_wii_crypt::params::Params;
use datboi_xf_wii_crypt::{
    DATA_LEN, GROUP_SECTORS, GroupHashes, Key, SECTOR_LEN, cbc_encrypt, decrypt_data,
    encrypt_sector, title_key,
};

/// The nix-built component (D66), embedded at compile time via
/// `DATBOI_COMPONENTS_DIR` — never a checked-in artifact.
const COMPONENT: &[u8] = include_bytes!(concat!(
    env!("DATBOI_COMPONENTS_DIR"),
    "/datboi_xf_wii_crypt.wasm"
));

/// blake3 of the fixture — the identity a recipe would pin.
const COMPONENT_BLAKE3: &str = "3c0f4c6bc4ed652375ffe5e4bc44dee652e5b3c574a7e4e60b88046a18fc0789";

/// A partition spanning two groups, the second partial (64 + 5 sectors).
const SECTORS: u64 = GROUP_SECTORS as u64 + 5;
const TITLE_ID: [u8; 8] = *b"\0\x01\0\0RIVE";

fn pattern(len: usize, salt: u32) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) ^ salt).to_le_bytes()[i % 4])
        .collect()
}

fn common_key() -> Key {
    pattern(16, 0xC0).try_into().unwrap()
}

/// The wrapped title key for a chosen plaintext title key.
fn wrapped_title_key(plain: &Key) -> Key {
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(&TITLE_ID);
    let mut w = *plain;
    cbc_encrypt(&common_key(), &iv, &mut w);
    w
}

fn params() -> (Params, Vec<u8>) {
    let plain_title_key: Key = pattern(16, 0x7E).try_into().unwrap();
    let p = Params {
        wrapped_title_key: wrapped_title_key(&plain_title_key),
        title_id: TITLE_ID,
        sectors: SECTORS,
    };
    let (buf, n) = p.encode();
    (p, buf[..n].to_vec())
}

/// Plaintext and its native encryption.
fn fixture() -> (Vec<u8>, Vec<u8>) {
    let (p, _) = params();
    let key = title_key(&common_key(), &p.wrapped_title_key, &p.title_id);
    let plain = pattern(usize::try_from(p.plain_len()).unwrap(), 0x5EC7);
    let mut enc = Vec::with_capacity(usize::try_from(p.encrypted_len()).unwrap());
    let mut sector = [0u8; SECTOR_LEN];
    for (g, group) in plain.chunks(DATA_LEN * GROUP_SECTORS).enumerate() {
        let hashes = GroupHashes::compute(group);
        for (s, data) in group.chunks(DATA_LEN).enumerate() {
            encrypt_sector(&key, &hashes, s, data, &mut sector);
            enc.extend_from_slice(&sector);
            let _ = g;
        }
    }
    // Native self-check: decrypt gives the plaintext back.
    let mut back = vec![0u8; DATA_LEN];
    decrypt_data(&key, enc[..SECTOR_LEN].try_into().unwrap(), &mut back);
    assert_eq!(back, plain[..DATA_LEN]);
    (plain, enc)
}

#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<u8>>>);

impl Collector {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.0.lock().expect("collector"))
    }
}

impl Write for Collector {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("collector").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// In-memory random-access input.
struct Mem(Vec<u8>);

impl RangeRead for Mem {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = usize::try_from(offset.min(self.0.len() as u64)).unwrap();
        let n = (self.0.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&self.0[start..start + n]);
        Ok(n)
    }
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

static SHARED: LazyLock<(StreamHost, StreamTransform)> = LazyLock::new(|| {
    let host = StreamHost::new(Limits::default()).expect("deterministic config accepted");
    let transform = host.load(COMPONENT).expect("fixture compiles");
    (host, transform)
});

fn shared() -> (&'static StreamHost, &'static StreamTransform) {
    let (h, t) = &*SHARED;
    (h, t)
}

fn seq_inputs(body: Vec<u8>) -> Vec<datboi_runtime::stream::StreamInput> {
    use datboi_runtime::stream::{SequentialInput, StreamInput};
    let key = common_key().to_vec();
    vec![
        StreamInput::Sequential(SequentialInput {
            len: body.len() as u64,
            reader: Box::new(std::io::Cursor::new(body)),
        }),
        StreamInput::Sequential(SequentialInput {
            len: key.len() as u64,
            reader: Box::new(std::io::Cursor::new(key)),
        }),
    ]
}

fn random_inputs(body: Vec<u8>) -> Vec<Box<dyn RangeRead>> {
    vec![Box::new(Mem(body)), Box::new(Mem(common_key().to_vec()))]
}

#[test]
fn component_bytes_are_pinned() {
    assert_eq!(
        blake3::hash(COMPONENT).to_hex().as_str(),
        COMPONENT_BLAKE3,
        "fixture changed: re-pin the golden constants"
    );
}

#[test]
fn describe_reports_affine_for_both_ops() {
    let (host, transform) = shared();
    for op in ["encrypt", "decrypt"] {
        let d = host.describe(transform, op).expect("describe");
        assert_eq!(d.seek, SeekClass::Affine);
        assert!(d.random_access_inputs.is_empty());
    }
    assert!(host.describe(transform, "hash").is_err(), "no such op");
}

/// wasm encrypt == native encrypt (both groups, the partial one
/// included), twice (determinism); wasm decrypt inverts it.
#[test]
fn run_matches_native_both_ways() {
    let (host, transform) = shared();
    let (_, params) = params();
    let (plain, enc) = fixture();
    for _ in 0..2 {
        let out = Collector::default();
        host.run(
            transform,
            "encrypt",
            &params,
            seq_inputs(plain.clone()),
            vec![Box::new(out.clone())],
        )
        .expect("encrypt succeeds");
        assert_eq!(out.take(), enc, "wasm ciphertext equals native ciphertext");
    }
    let out = Collector::default();
    host.run(
        transform,
        "decrypt",
        &params,
        seq_inputs(enc.clone()),
        vec![Box::new(out.clone())],
    )
    .expect("decrypt succeeds");
    assert_eq!(out.take(), plain, "wasm decrypt inverts");
}

/// D49 seek-equivalence at both quanta: windows inside a sector, across
/// sectors, across the group boundary, into the partial group, EOF
/// clamp, past EOF, whole.
#[test]
fn served_ranges_match_materialization() {
    let (host, transform) = shared();
    let (_, params) = params();
    let (plain, enc) = fixture();
    let sector = SECTOR_LEN as u64;
    let group = sector * GROUP_SECTORS as u64;
    let enc_len = enc.len() as u64;
    for (offset, wlen) in [
        (0u64, 1u64),
        (0x3D0, 0x30),
        (sector - 1, 2),
        (3 * sector + 7, 2 * sector),
        (group - 100, 200),
        (group + 2 * sector + 5, sector),
        (enc_len - 10, 100),
        (enc_len + 7, 4),
        (0, enc_len),
    ] {
        let out = Collector::default();
        host.serve_range(
            transform,
            "encrypt",
            &params,
            random_inputs(plain.clone()),
            RangeRequest {
                output_ix: 0,
                offset,
                len: wlen,
            },
            Box::new(out.clone()),
        )
        .expect("serve encrypt");
        let start = usize::try_from(offset.min(enc_len)).unwrap();
        let end = usize::try_from(offset.saturating_add(wlen).min(enc_len)).unwrap();
        assert_eq!(
            out.take(),
            &enc[start..end],
            "encrypt window {offset}+{wlen}"
        );
    }
    let data = DATA_LEN as u64;
    let plain_len = plain.len() as u64;
    for (offset, wlen) in [
        (0u64, 1u64),
        (data - 1, 2),
        (5 * data + 100, 3 * data),
        (plain_len - 10, 100),
        (plain_len + 1, 4),
        (0, plain_len),
    ] {
        let out = Collector::default();
        host.serve_range(
            transform,
            "decrypt",
            &params,
            random_inputs(enc.clone()),
            RangeRequest {
                output_ix: 0,
                offset,
                len: wlen,
            },
            Box::new(out.clone()),
        )
        .expect("serve decrypt");
        let start = usize::try_from(offset.min(plain_len)).unwrap();
        let end = usize::try_from(offset.saturating_add(wlen).min(plain_len)).unwrap();
        assert_eq!(
            out.take(),
            &plain[start..end],
            "decrypt window {offset}+{wlen}"
        );
    }
}

#[test]
fn malformed_params_and_inputs_are_guest_errors() {
    let (host, transform) = shared();
    let (_, params) = params();
    let (plain, _) = fixture();
    let err = host
        .run(
            transform,
            "encrypt",
            &[0xa3, 0x01, 0x40],
            seq_inputs(plain.clone()),
            vec![Box::new(Collector::default())],
        )
        .expect_err("malformed params refuse");
    assert!(matches!(err, RuntimeError::Transform(_)), "{err:?}");
    // A key of the wrong length.
    use datboi_runtime::stream::{SequentialInput, StreamInput};
    let err = host
        .run(
            transform,
            "encrypt",
            &params,
            vec![
                StreamInput::Sequential(SequentialInput {
                    len: plain.len() as u64,
                    reader: Box::new(std::io::Cursor::new(plain.clone())),
                }),
                StreamInput::Sequential(SequentialInput {
                    len: 15,
                    reader: Box::new(std::io::Cursor::new(vec![0u8; 15])),
                }),
            ],
            vec![Box::new(Collector::default())],
        )
        .expect_err("short key refuses");
    assert!(
        matches!(err, RuntimeError::Transform(ref m) if m.contains("16 bytes")),
        "{err:?}"
    );
    // Plaintext length disagreeing with the params.
    let err = host
        .run(
            transform,
            "encrypt",
            &params,
            seq_inputs(plain[..DATA_LEN].to_vec()),
            vec![Box::new(Collector::default())],
        )
        .expect_err("length mismatch refuses");
    assert!(
        matches!(err, RuntimeError::Transform(ref m) if m.contains("params describe")),
        "{err:?}"
    );
}
