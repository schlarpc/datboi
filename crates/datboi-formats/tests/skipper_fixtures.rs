//! Golden tests: realistic detector XMLs against synthetic roms.

use datboi_formats::skipper::{Detector, Operation};

const NES: &str = include_str!("fixtures/nes.xml");
const FDS: &str = include_str!("fixtures/fds.xml");
const LYNX: &str = include_str!("fixtures/lynx.xml");

fn ines_rom() -> Vec<u8> {
    // 16-byte iNES header ("NES\x1a", 2×16K PRG, zero padding) + PRG data.
    let mut rom = vec![0u8; 16 + 32 * 1024];
    rom[..4].copy_from_slice(b"NES\x1a");
    rom[4] = 2;
    for (i, b) in rom[16..].iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    rom
}

#[test]
fn nes_detector_skips_ines_header() {
    let detector = Detector::parse(NES.as_bytes()).expect("parses");
    assert_eq!(detector.name, "Nintendo - Nintendo Entertainment System");

    let rom = ines_rom();
    let decision = detector.evaluate(&rom).expect("headered rom matches");
    assert_eq!(decision.start, 0x10);
    assert_eq!(decision.end, rom.len() as u64);
    assert_eq!(decision.operation, Operation::None);
    assert_eq!(decision.apply(&rom), &rom[16..]);

    // Headerless dump: no match — whole file is the real data.
    assert_eq!(detector.evaluate(&rom[16..]), None);
}

#[test]
fn fds_detector_skips_fwnes_header() {
    let detector = Detector::parse(FDS.as_bytes()).expect("parses");
    let mut disk = vec![0u8; 16 + 65_500];
    disk[..4].copy_from_slice(b"FDS\x1a");
    let decision = detector.evaluate(&disk).expect("matches");
    assert_eq!((decision.start, decision.end), (0x10, disk.len() as u64));
    // Raw disk image starting with the on-media magic "\x01*NINTENDO-HVC*"
    // has no fwNES header and must not match.
    let mut raw = vec![0u8; 65_500];
    raw[0] = 0x01;
    raw[1..15].copy_from_slice(b"*NINTENDO-HVC*");
    assert_eq!(detector.evaluate(&raw), None);
}

#[test]
fn lynx_detector_skips_lnx_header() {
    let detector = Detector::parse(LYNX.as_bytes()).expect("parses");
    let mut rom = vec![0u8; 0x40 + 256 * 1024];
    rom[..4].copy_from_slice(b"LYNX");
    let decision = detector.evaluate(&rom).expect("matches");
    assert_eq!(decision.start, 0x40);
    assert_eq!(decision.apply(&rom).len(), 256 * 1024);
    assert_eq!(detector.evaluate(&rom[0x40..]), None);
}
