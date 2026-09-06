use super::*;

/// FIPS-197 C.1: AES-128 single block.
#[test]
fn aes_known_answer() {
    let key: Key = core::array::from_fn(|i| i as u8);
    let mut block = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    cbc_encrypt(&key, &[0u8; 16], &mut block);
    assert_eq!(
        block,
        [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a
        ]
    );
    cbc_decrypt(&key, &[0u8; 16], &mut block);
    assert_eq!(block[..4], [0x00, 0x11, 0x22, 0x33]);
}

/// NIST SP 800-38A F.2.1: AES-128-CBC, four blocks.
#[test]
fn cbc_known_answer() {
    let key: Key = [
        0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f,
        0x3c,
    ];
    let iv: Key = core::array::from_fn(|i| i as u8);
    let plain: [u8; 64] = [
        0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93, 0x17,
        0x2a, 0xae, 0x2d, 0x8a, 0x57, 0x1e, 0x03, 0xac, 0x9c, 0x9e, 0xb7, 0x6f, 0xac, 0x45, 0xaf,
        0x8e, 0x51, 0x30, 0xc8, 0x1c, 0x46, 0xa3, 0x5c, 0xe4, 0x11, 0xe5, 0xfb, 0xc1, 0x19, 0x1a,
        0x0a, 0x52, 0xef, 0xf6, 0x9f, 0x24, 0x45, 0xdf, 0x4f, 0x9b, 0x17, 0xad, 0x2b, 0x41, 0x7b,
        0xe6, 0x6c, 0x37, 0x10,
    ];
    let mut buf = plain;
    cbc_encrypt(&key, &iv, &mut buf);
    assert_eq!(
        buf[..16],
        [
            0x76, 0x49, 0xab, 0xac, 0x81, 0x19, 0xb2, 0x46, 0xce, 0xe9, 0x8e, 0x9b, 0x12, 0xe9,
            0x19, 0x7d
        ]
    );
    assert_eq!(
        buf[48..],
        [
            0x3f, 0xf1, 0xca, 0xa1, 0x68, 0x1f, 0xac, 0x09, 0x12, 0x0e, 0xca, 0x30, 0x75, 0x86,
            0xe1, 0xa7
        ]
    );
    cbc_decrypt(&key, &iv, &mut buf);
    assert_eq!(buf, plain);
}

/// FIPS 180-1 "abc".
#[test]
fn sha1_known_answer() {
    assert_eq!(
        sha1(b"abc"),
        [
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
        ]
    );
}

fn pattern(len: usize, salt: u32) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) ^ salt).to_le_bytes()[i % 4])
        .collect()
}

#[test]
fn title_key_unwrap_round_trips() {
    let common: Key = pattern(16, 0xC0).try_into().unwrap();
    let title_id = *b"\0\x01\0\0RIVE";
    let plain_title_key: Key = pattern(16, 0x7E).try_into().unwrap();
    let mut wrapped = plain_title_key;
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(&title_id);
    cbc_encrypt(&common, &iv, &mut wrapped);
    assert_eq!(title_key(&common, &wrapped, &title_id), plain_title_key);
}

/// The hash block layout: H0s at 0, H1s at 0x280, H2s at 0x340, zero
/// padding everywhere else; a partial group hashes its missing sectors
/// as zero data.
#[test]
fn hash_block_layout_and_partial_groups() {
    let data = pattern(3 * DATA_LEN, 1);
    let hashes = GroupHashes::compute(&data);
    let mut block = [0u8; HASHES_LEN];
    hashes.block(2, &mut block);
    // H0[0] of sector 2 is the sha1 of its first KiB.
    assert_eq!(
        block[..20],
        sha1(&data[2 * DATA_LEN..2 * DATA_LEN + HASHES_LEN])
    );
    assert!(block[0x26C..0x280].iter().all(|&b| b == 0));
    assert_eq!(block[H1_OFFSET..H1_OFFSET + 20], hashes.h1[0]);
    assert_eq!(block[H1_OFFSET + 40..H1_OFFSET + 60], hashes.h1[2]);
    assert!(block[0x320..0x340].iter().all(|&b| b == 0));
    assert_eq!(block[H2_OFFSET..H2_OFFSET + 20], hashes.h2[0]);
    assert!(block[0x3E0..].iter().all(|&b| b == 0));
    // Sector 3 is absent: its H1 is the hash of 31 zero-KiB hashes.
    let zero = sha1(&[0u8; HASHES_LEN]);
    let expect_h1 = sha1([zero; H0_PER_SECTOR].as_flattened());
    assert_eq!(hashes.h1[3], expect_h1);
    assert_eq!(hashes.h1[63], expect_h1);
    assert_eq!(hashes.h3, sha1(hashes.h2.as_flattened()));
    let mut plain = [0u8; SECTOR_LEN];
    plain[..HASHES_LEN].copy_from_slice(&block);
    assert!(hash_block_matches(&plain, &hashes, 2));
}

/// encrypt → decrypt is the identity on data AND reproduces the hash
/// block; the data IV is the encrypted block's `0x3D0..0x3E0`.
#[test]
fn sector_round_trip() {
    let key: Key = pattern(16, 0x11).try_into().unwrap();
    let data = pattern(2 * DATA_LEN, 7);
    let hashes = GroupHashes::compute(&data);
    let mut enc = [0u8; SECTOR_LEN];
    encrypt_sector(&key, &hashes, 1, &data[DATA_LEN..], &mut enc);
    let mut plain = [0u8; SECTOR_LEN];
    decrypt_sector(&key, &enc, &mut plain);
    assert_eq!(&plain[HASHES_LEN..], &data[DATA_LEN..]);
    assert!(hash_block_matches(&plain, &hashes, 1));
    assert!(
        !hash_block_matches(&plain, &hashes, 0),
        "wrong sector's tree"
    );
    let mut only = [0u8; DATA_LEN];
    decrypt_data(&key, &enc, &mut only);
    assert_eq!(&only[..], &data[DATA_LEN..]);
    // Determinism: the same inputs produce the same ciphertext.
    let mut again = [0u8; SECTOR_LEN];
    encrypt_sector(&key, &hashes, 1, &data[DATA_LEN..], &mut again);
    assert_eq!(enc, again);
}
