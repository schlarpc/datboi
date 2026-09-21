use std::io::Cursor;

use super::synth::{SynthSpec, build, synth_v5};
use super::*;

/// Data that compresses: a low-amplitude waveform, which every codec
/// here beats storing raw (FLAC included — random bytes would make its
/// residuals wider than the samples, and the writer would fall back to
/// raw hunks, silently skipping the codec under test).
fn wave(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut phase = 0i32;
    while out.len() < len {
        phase = (phase + 37) % 512;
        let v = i16::try_from(phase - 256).expect("small");
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.truncate(len);
    out
}

/// A run of real CD frames: mode 1 sectors with regenerable sync and
/// parity, each followed by 96 subcode bytes.
fn cd_frames(frames: usize) -> Vec<u8> {
    const SYNC: [u8; 12] = [
        0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00,
    ];
    let payload = wave(frames * 2048);
    let mut out = Vec::with_capacity(frames * codec::CD_FRAME_SIZE);
    for f in 0..frames {
        let mut sector = [0u8; codec::CD_MAX_SECTOR_DATA];
        sector[..12].copy_from_slice(&SYNC);
        // Minute/second/frame address, BCD, starting at 00:02:00.
        sector[12] = 0x00;
        sector[13] = 0x02;
        sector[14] = u8::try_from(f % 10).expect("small");
        sector[15] = 1; // mode 1
        sector[16..2064].copy_from_slice(&payload[f * 2048..(f + 1) * 2048]);
        let edc = datboi_xf_ecm::edc(&sector[..2064]);
        sector[2064..2068].copy_from_slice(&edc.to_le_bytes());
        codec::ecc_generate(&mut sector);
        out.extend_from_slice(&sector);
        out.extend(std::iter::repeat_n(0u8, codec::CD_MAX_SUBCODE_DATA));
    }
    out
}

fn verified(bytes: &[u8]) -> Verification {
    let v = verify(Cursor::new(bytes), &mut |_| {}).expect("verifies");
    assert!(v.declaration_holds, "{:?}", v.mismatch);
    v
}

#[test]
fn parses_v5_fields() {
    let bytes = synth_v5(1 << 30, [0xAA; 20], [0xBB; 20]);
    let h = parse_header(&bytes).expect("is a chd");
    assert_eq!(h.version, 5);
    assert_eq!(h.logical_bytes, 1 << 30);
    assert_eq!(h.raw_sha1, Some([0xAA; 20]));
    assert_eq!(h.combined_sha1, Some([0xBB; 20]));
    assert_eq!(h.declared_disk_sha1(), Some([0xBB; 20]));
    assert!(!h.has_parent());
}

#[test]
fn not_a_chd_at_all() {
    assert!(try_parse_header(b"PK\x03\x04 not a chd").is_none());
    assert!(try_parse_header(b"MCompr").is_none()); // truncated magic
}

/// D44 left v1–v4 opaque. They parse now — and each version's digests
/// are reported for what they actually cover, never promoted.
#[test]
fn every_version_reports_its_own_digest_shape() {
    for version in 1..=5u32 {
        let spec = SynthSpec::new(version, CODEC_ZLIB);
        let data = wave(4096 * 3);
        let bytes = build(&spec, &data);
        let h = parse_header(&bytes).expect("is a chd");
        assert_eq!(h.version, version);
        assert_eq!(h.logical_bytes, data.len() as u64);
        assert_eq!(h.hunk_bytes, 4096);
        match version {
            // No sha1 existed yet: a v1/v2 CHD can never answer a
            // modern MAME disk claim, and says so rather than
            // offering some other digest in its place.
            1 | 2 => {
                assert!(h.raw_md5.is_some());
                assert_eq!(h.raw_sha1, None);
                assert_eq!(h.declared_disk_sha1(), None);
            }
            // v3 declares a sha1 of the RAW data only.
            3 => {
                assert!(h.raw_md5.is_some());
                assert_eq!(h.combined_sha1, None);
                assert_eq!(h.declared_disk_sha1(), h.raw_sha1);
            }
            // v4/v5 declare the combined raw+metadata digest dats use.
            _ => {
                assert!(h.combined_sha1.is_some());
                assert_ne!(h.combined_sha1, h.raw_sha1);
                assert_eq!(h.declared_disk_sha1(), h.combined_sha1);
            }
        }
    }
}

#[test]
fn a_header_that_contradicts_itself_is_refused() {
    let mut bytes = build(&SynthSpec::new(4, CODEC_ZLIB), &wave(4096));
    bytes[8..12].copy_from_slice(&99u32.to_be_bytes());
    let err = parse_header(&bytes).expect_err("self-contradictory");
    assert!(matches!(err, ChdError::Malformed(_)), "{err:?}");

    let mut bytes = build(&SynthSpec::new(5, CODEC_ZLIB), &wave(4096));
    bytes[12..16].copy_from_slice(&6u32.to_be_bytes());
    let err = parse_header(&bytes).expect_err("unknown version");
    assert!(
        matches!(err, ChdError::Malformed(ref m) if m.contains("version 6")),
        "{err:?}"
    );
}

/// The round trip that matters: build a CHD at every version, decode
/// every hunk, and confirm the digests we computed are the ones the
/// file declares.
#[test]
fn round_trips_every_version() {
    for version in 1..=5u32 {
        for &codec in &[CODEC_NONE, CODEC_ZLIB] {
            let spec = SynthSpec::new(version, codec);
            // Deliberately not a whole number of hunks: the last one
            // is padded, and the digests must cover only real bytes.
            let data = wave(4096 * 3 + 2048);
            let bytes = build(&spec, &data);
            let v = verified(&bytes);
            assert_eq!(v.header.version, version, "v{version} codec {codec:#x}");
            assert_eq!(v.raw_sha1, sha1_of(&data));
            if version >= 4 {
                assert_eq!(v.combined_sha1, v.header.combined_sha1);
            }
        }
    }
}

#[test]
fn round_trips_every_v5_codec() {
    for &codec in &[CODEC_ZLIB, CODEC_LZMA, CODEC_HUFFMAN, CODEC_FLAC] {
        let data = wave(4096 * 3 + 1024);
        let bytes = build(&SynthSpec::new(5, codec), &data);
        let v = verified(&bytes);
        assert_eq!(v.raw_sha1, sha1_of(&data), "codec {}", codec_name(codec));
        // The map must actually have chosen the codec under test, or
        // the fixture proved nothing.
        let reader = ChdReader::open(Cursor::new(&bytes)).expect("opens");
        assert!(
            reader
                .map()
                .iter()
                .any(|e| e.kind == HunkKind::Codec(codec)),
            "no hunk used {}",
            codec_name(codec)
        );
    }
}

/// A run of audio frames: 2352 bytes of samples, no sync and no parity
/// — what `cdfl` is actually for (it is the CDDA variant, and unlike
/// its siblings it carries no ECC bitmap at all).
fn cd_audio_frames(frames: usize) -> Vec<u8> {
    let samples = wave(frames * codec::CD_MAX_SECTOR_DATA);
    let mut out = Vec::with_capacity(frames * codec::CD_FRAME_SIZE);
    for f in 0..frames {
        out.extend_from_slice(
            &samples[f * codec::CD_MAX_SECTOR_DATA..(f + 1) * codec::CD_MAX_SECTOR_DATA],
        );
        out.extend(std::iter::repeat_n(0u8, codec::CD_MAX_SUBCODE_DATA));
    }
    out
}

#[test]
fn round_trips_the_cd_codecs() {
    for &codec in &[CODEC_CD_ZLIB, CODEC_CD_LZMA, CODEC_CD_FLAC] {
        let data = if codec == CODEC_CD_FLAC {
            cd_audio_frames(24)
        } else {
            cd_frames(24)
        };
        let bytes = build(&SynthSpec::cd(codec), &data);
        let v = verified(&bytes);
        assert_eq!(v.raw_sha1, sha1_of(&data), "codec {}", codec_name(codec));
        let reader = ChdReader::open(Cursor::new(&bytes)).expect("opens");
        assert!(
            reader
                .map()
                .iter()
                .any(|e| e.kind == HunkKind::Codec(codec)),
            "no hunk used {}",
            codec_name(codec)
        );
    }
}

/// D44 ruled strictness for exactly this case: a CHD whose header is
/// intact but whose data is not there must NOT verify. It is also the
/// case a header-only read cannot tell from a good file.
#[test]
fn a_truncated_chd_with_an_intact_header_fails_verification() {
    let data = wave(4096 * 4);
    let bytes = build(&SynthSpec::new(5, CODEC_ZLIB), &data);
    // The header still parses, and still declares the real disk sha1.
    let truncated = &bytes[..bytes.len() * 2 / 3];
    let h = parse_header(truncated).expect("header survives");
    assert_eq!(
        h.declared_disk_sha1(),
        parse_header(&bytes).unwrap().declared_disk_sha1()
    );

    let err = verify(Cursor::new(truncated), &mut |_| {}).expect_err("must not verify");
    assert!(err.is_conclusion(), "{err:?}");
    assert!(matches!(err, ChdError::Malformed(_)), "{err:?}");
}

/// Truncating a v1-era CHD lands in the map rather than the payload;
/// it must be just as conclusive.
#[test]
fn a_truncated_legacy_chd_fails_in_the_map() {
    let bytes = build(&SynthSpec::new(3, CODEC_ZLIB), &wave(4096 * 8));
    let err = verify(Cursor::new(&bytes[..100]), &mut |_| {}).expect_err("must not verify");
    assert!(matches!(err, ChdError::Malformed(_)), "{err:?}");
}

/// A CHD whose bytes are fine but whose declaration is wrong: decoding
/// succeeds, and the verdict says the file lied. Never an error — the
/// analyzer turns this into a Negative with detail (D81).
#[test]
fn a_lying_declaration_is_a_verdict_not_an_error() {
    let mut bytes = build(&SynthSpec::new(5, CODEC_ZLIB), &wave(4096 * 2));
    bytes[84] ^= 0xff; // flip a bit of the declared combined sha1
    let v = verify(Cursor::new(&bytes), &mut |_| {}).expect("decodes fine");
    assert!(!v.declaration_holds);
    assert!(
        v.mismatch
            .expect("detail")
            .contains("data+metadata hash to")
    );
}

/// Flipping a payload byte must be caught even before the sha1: the
/// map's own per-hunk checksum covers it. Both kinds of stored hunk —
/// an uncompressed one is no less capable of being corrupt, and it is
/// the one whose checksum is easiest to forget to check.
#[test]
fn a_corrupt_hunk_is_caught_by_the_maps_checksum() {
    for (label, bytes) in [
        (
            "v4, compressed hunks (crc32)",
            build(&SynthSpec::new(4, CODEC_ZLIB), &wave(4096 * 2)),
        ),
        (
            // Incompressible data: every hunk stores raw, so the map's
            // entries are COMPRESSION_NONE carrying a crc16.
            "v5, uncompressed hunks (crc16)",
            build(&SynthSpec::new(5, CODEC_ZLIB), &incompressible(4096 * 2)),
        ),
    ] {
        // Corrupt a byte of the LAST hunk's stored payload, found
        // through the map rather than guessed at from the file's
        // length — the metadata chain lives at the end, and an offset
        // that lands there tests the metadata reader instead.
        let reader = ChdReader::open(Cursor::new(&bytes)).expect("opens");
        let last = *reader.map().last().expect("hunks");
        let at = usize::try_from(last.offset).expect("offset") + last.length as usize / 2;
        let mut corrupt = bytes.clone();
        corrupt[at] ^= 0x01;
        let err = verify(Cursor::new(&corrupt), &mut |_| {}).expect_err("must not verify");
        assert!(matches!(err, ChdError::Malformed(_)), "{label}: {err:?}");
        assert!(err.to_string().contains("the map says"), "{label}: {err}");
        // The intact original still verifies, so the test is about the
        // corruption and not about the fixture.
        verify(Cursor::new(&bytes), &mut |_| {}).expect(label);
    }
}

/// Every one of these decoders is fed bytes off the internet. A
/// corrupt hunk may fail any way it likes, but it may never panic —
/// an analyzer that aborts the process on one bad file is worse than
/// one that reports it. Cheap stand-in for the fuzz targets
/// open-questions.md still owes.
#[test]
fn corruption_never_panics_whatever_the_codec() {
    for &codec in &[
        CODEC_ZLIB,
        CODEC_LZMA,
        CODEC_HUFFMAN,
        CODEC_FLAC,
        CODEC_CD_ZLIB,
        CODEC_CD_LZMA,
        CODEC_CD_FLAC,
    ] {
        let (spec, data) = if codec == CODEC_CD_FLAC {
            (SynthSpec::cd(codec), cd_audio_frames(8))
        } else if matches!(codec, CODEC_CD_ZLIB | CODEC_CD_LZMA) {
            (SynthSpec::cd(codec), cd_frames(8))
        } else {
            (SynthSpec::new(5, codec), wave(4096 * 2))
        };
        let bytes = build(&spec, &data);
        // Walk a deterministic spread of byte positions, flipping one
        // at a time: headers, map, payloads and metadata all get hit.
        let step = (bytes.len() / 97).max(1);
        for at in (0..bytes.len()).step_by(step) {
            let mut corrupt = bytes.clone();
            corrupt[at] ^= 0xa5;
            // The only requirement is that this returns.
            let _ = verify(Cursor::new(&corrupt), &mut |_| {});
        }
    }
}

/// Bytes no codec beats, so the writer stores them raw.
fn incompressible(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Codecs we refuse are named, not skipped — and the refusal happens
/// before any work, so nothing is ever half-verified.
#[test]
fn an_unsupported_codec_is_refused_by_name() {
    let mut bytes = build(&SynthSpec::new(5, CODEC_ZLIB), &wave(4096));
    bytes[16..20].copy_from_slice(&CODEC_AVHUFF.to_be_bytes());
    let err = verify(Cursor::new(&bytes), &mut |_| {}).expect_err("cannot decode");
    assert!(
        matches!(err, ChdError::Unsupported(ref m) if m.contains("avhu")),
        "{err:?}"
    );
    assert!(err.is_conclusion());
}

/// A delta CHD's data is in a file we do not have. Refused by name,
/// never reported as verified against the parent's bytes.
#[test]
fn a_delta_chd_is_refused() {
    let mut bytes = build(&SynthSpec::new(5, CODEC_ZLIB), &wave(4096));
    bytes[104..124].copy_from_slice(&[7u8; 20]);
    let h = parse_header(&bytes).expect("parses");
    assert!(h.has_parent());
    let err = verify(Cursor::new(&bytes), &mut |_| {}).expect_err("needs a parent");
    assert!(
        matches!(err, ChdError::Unsupported(ref m) if m.contains("parent")),
        "{err:?}"
    );
}

/// The combined digest folds metadata in by sorted `(tag, sha1)`
/// record, so the order entries appear in the file is not part of the
/// identity — which is what lets chdman rewrite the chain.
#[test]
fn metadata_order_does_not_change_the_combined_digest() {
    let data = wave(4096);
    let entries = vec![
        (fourcc(b"GDDD"), 1u8, b"first".to_vec()),
        (fourcc(b"CHT2"), 1, b"second".to_vec()),
        (fourcc(b"IDNT"), 0, b"not checksummed".to_vec()),
    ];
    let mut a = SynthSpec::new(5, CODEC_ZLIB);
    a.metadata.clone_from(&entries);
    let mut b = SynthSpec::new(5, CODEC_ZLIB);
    b.metadata = entries.into_iter().rev().collect();

    let va = verified(&build(&a, &data));
    let vb = verified(&build(&b, &data));
    assert_eq!(va.combined_sha1, vb.combined_sha1);
    assert_ne!(va.combined_sha1, Some(va.raw_sha1));
}

#[test]
fn progress_reports_logical_bytes() {
    let data = wave(4096 * 5);
    let bytes = build(&SynthSpec::new(5, CODEC_ZLIB), &data);
    let mut seen = Vec::new();
    verify(Cursor::new(&bytes), &mut |n| seen.push(n)).expect("verifies");
    assert_eq!(seen.last(), Some(&(data.len() as u64)));
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "monotonic");
}

fn sha1_of(data: &[u8]) -> [u8; 20] {
    use sha1::Digest as _;
    sha1::Sha1::digest(data).into()
}
