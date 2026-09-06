use super::*;

/// Dolphin's / nod's vectors: (id, disc, offset, first bytes).
const VECTORS: &[([u8; 4], u8, u64, &[u8])] = &[
    (
        *b"GALE",
        0,
        0x60_0000,
        &[
            0xE9, 0x47, 0x67, 0xBD, 0x41, 0x50, 0x4D, 0x5D, 0x61, 0x48, 0xB1, 0x99, 0xA0, 0x12,
            0x0C, 0xBA,
        ],
    ),
    (
        *b"GALE",
        0,
        0x60_8000,
        &[
            0xE2, 0xBB, 0xBD, 0x77, 0xDA, 0xB2, 0x22, 0x42, 0x1C, 0x0C, 0x0B, 0xFC, 0xAC, 0x06,
            0xEA, 0xD0,
        ],
    ),
    (
        *b"GPIE",
        0,
        0x32_2904,
        &[
            0x97, 0xD8, 0x23, 0x0B, 0x12, 0xAA, 0x20, 0x45, 0xC2, 0xBD, 0x71, 0x8C, 0x30, 0x32,
            0xC5, 0x2F,
        ],
    ),
    (
        *b"GM8E",
        0,
        0x2_7FF0,
        &[
            0xAD, 0x6F, 0x21, 0xBE, 0x05, 0x57, 0x10, 0xED, 0xEA, 0xB0, 0x8E, 0xFD, 0x91, 0x58,
            0xA2, 0x0E, 0xDC, 0x0D, 0x59, 0xC0, 0x02, 0x98, 0xA5, 0x00, 0x39, 0x5B, 0x68, 0xA6,
            0x5D, 0x53, 0x2D, 0xB6,
        ],
    ),
];

#[test]
fn reproduces_the_reference_vectors() {
    let mut lfg = Lfg::default();
    for (id, disc, offset, want) in VECTORS {
        let mut got = vec![0u8; want.len()];
        fill_at(&mut lfg, *id, *disc, *offset, &mut got);
        assert_eq!(&got, want, "{} @ {offset:#x}", String::from_utf8_lossy(id));
    }
}

/// A fill across a sector boundary equals the two per-sector fills
/// concatenated, and byte-granular seeks agree with a bulk fill.
#[test]
fn positional_and_boundary_consistent() {
    let id = *b"GM8E";
    let mut lfg = Lfg::default();
    let mut bulk = vec![0u8; 3 * SECTOR_LEN + 100];
    fill_at(&mut lfg, id, 1, SECTOR_LEN as u64 - 50, &mut bulk);
    for (offset, len) in [
        (0u64, 50usize),
        (50, 1),
        (51, SECTOR_LEN),
        (77, 4000),
        (bulk.len() as u64 - 3, 3),
    ] {
        let mut part = vec![0u8; len];
        fill_at(&mut lfg, id, 1, SECTOR_LEN as u64 - 50 + offset, &mut part);
        let o = usize::try_from(offset).unwrap();
        assert_eq!(part, &bulk[o..o + len], "window {offset}+{len}");
    }
    assert_eq!(
        matching_prefix(&mut lfg, id, 1, SECTOR_LEN as u64 - 50, &bulk),
        bulk.len()
    );
    bulk[SECTOR_LEN + 10] ^= 1;
    assert_eq!(
        matching_prefix(&mut lfg, id, 1, SECTOR_LEN as u64 - 50, &bulk),
        SECTOR_LEN + 10
    );
    // A different disc number is a different stream.
    let mut other = vec![0u8; 64];
    fill_at(&mut lfg, id, 2, SECTOR_LEN as u64 - 50, &mut other);
    assert_ne!(&other[..], &bulk[..64]);
}
