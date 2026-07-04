//! Golden-fixture integration tests for the dat parsers, exercising the
//! 60-dats gotcha list per format. cmpro fixtures are inline raw strings
//! (non-.xml files don't survive the flake's crane source filter).

use datboi_formats::model::{ClaimKind, ClaimStatus, DatFile, ParseError};
use datboi_formats::{cmpro, listxml, logiqx, parse, softlist};

const LOGIQX: &str = include_str!("fixtures/logiqx_nointro.xml");
const LISTXML: &str = include_str!("fixtures/listxml.xml");
const SOFTLIST: &str = include_str!("fixtures/softlist.xml");

fn parse_logiqx() -> DatFile {
    logiqx::parse(LOGIQX.as_bytes()).expect("fixture parses")
}

#[test]
fn logiqx_header_and_emitter_hints() {
    let dat = parse_logiqx();
    let h = &dat.header;
    assert_eq!(h.name.as_deref(), Some("Nintendo - Game Boy"));
    assert_eq!(h.version.as_deref(), Some("20260630-123456"));
    assert_eq!(h.author.as_deref(), Some("No-Intro"));
    assert_eq!(h.detector.as_deref(), Some("No-Intro_NES.xml"));
    assert_eq!(h.force_nodump.as_deref(), Some("required"));
    // Unknown attrs preserved with their emitter prefix.
    assert_eq!(
        h.attrs.get("clrmamepro:mood").map(String::as_str),
        Some("synthy")
    );
    assert_eq!(
        h.attrs.get("romcenter:plugin").map(String::as_str),
        Some("wips.dll")
    );
}

#[test]
fn logiqx_pc_extensions_and_hashes() {
    let dat = parse_logiqx();
    assert_eq!(dat.entries.len(), 3);

    let usa = &dat.entries[0];
    assert_eq!(usa.name, "Alpha (USA)");
    assert_eq!(usa.id.as_deref(), Some("0001"));
    assert_eq!(usa.attrs.get("mysterious").map(String::as_str), Some("yes"));
    assert_eq!(usa.releases.len(), 1);

    let full = &usa.claims[0];
    assert_eq!(full.crc32, Some([0x1b, 0x2c, 0x3d, 0x4e]));
    assert_eq!(full.md5, Some([0xaa; 16]));
    assert_eq!(full.sha1, Some([0xbb; 20]));
    assert_eq!(full.sha256, Some([0xcc; 32]));
    assert!(full.mia);
    assert_eq!(
        full.attrs.get("serial").map(String::as_str),
        Some("DMG-AA-USA")
    );

    // Zero-byte rom is legal (gotcha 4).
    let empty = &usa.claims[1];
    assert_eq!(empty.size, Some(0));
    assert_eq!(empty.crc32, Some([0; 4]));

    // Short crc left-padded (gotcha 2).
    assert_eq!(usa.claims[2].crc32, Some([0x00, 0x00, 0x0a, 0xbc]));
}

#[test]
fn logiqx_clones_statuses_and_duplicates() {
    let dat = parse_logiqx();
    let eur = &dat.entries[1];
    assert_eq!(eur.cloneof.as_deref(), Some("Alpha (USA)"));
    assert_eq!(eur.cloneof_id.as_deref(), Some("0001"));
    assert_eq!(
        eur.attrs.get("comment").map(String::as_str),
        Some("first comment\nsecond comment")
    );
    assert_eq!(eur.releases.len(), 2);
    assert!(eur.releases[1].is_default);
    assert_eq!(eur.releases[1].language.as_deref(), Some("fr"));

    // Duplicate hashes under different names stay distinct rows (gotcha 4).
    assert_eq!(eur.claims[0].crc32, eur.claims[1].crc32);
    assert_ne!(eur.claims[0].name, eur.claims[1].name);
    assert_eq!(eur.claims[0].status, ClaimStatus::BadDump);
    assert_eq!(
        eur.claims[0].attrs.get("weird").map(String::as_str),
        Some("attr")
    );

    // nodump carries no hashes and can never be satisfied (gotcha 5).
    let lost = &eur.claims[2];
    assert_eq!(lost.status, ClaimStatus::NoDump);
    assert_eq!(lost.crc32, None);

    // machine element synonym + bios flag + biosset + sample.
    let bios = &dat.entries[2];
    assert!(bios.is_bios);
    assert_eq!(
        bios.attrs.get("biosset:euro").map(String::as_str),
        Some("Euro BIOS [default]")
    );
    assert_eq!(bios.claims[0].status, ClaimStatus::Verified);
    assert_eq!(bios.claims[1].kind, ClaimKind::Sample);
    assert_eq!(bios.claims[1].name, "boing");
}

#[test]
fn listxml_machines() {
    let dat = listxml::parse(LISTXML.as_bytes()).expect("fixture parses");
    assert_eq!(dat.header.name.as_deref(), Some("MAME"));
    assert_eq!(dat.header.version.as_deref(), Some("0.270 (synthetic)"));
    assert_eq!(dat.entries.len(), 3);

    let bios = &dat.entries[0];
    assert!(bios.is_bios);
    assert!(!bios.runnable);
    let biosrom = &bios.claims[0];
    assert_eq!(biosrom.attrs.get("bios").map(String::as_str), Some("euro"));
    assert_eq!(
        biosrom.attrs.get("region").map(String::as_str),
        Some("mainbios")
    );

    let mslug = &dat.entries[1];
    assert_eq!(mslug.romof.as_deref(), Some("neogeo"));
    assert_eq!(mslug.device_refs, ["z80", "ym2610"]);
    // MAME roms: crc+sha1, never md5 (gotcha 2).
    assert!(mslug.claims[0].md5.is_none());
    assert!(mslug.claims[0].sha1.is_some());
    let nodump = &mslug.claims[1];
    assert_eq!(nodump.status, ClaimStatus::NoDump);
    assert!(nodump.optional);
    let sample = &mslug.claims[2];
    assert_eq!(sample.kind, ClaimKind::Sample);
    // Disk claims: internal sha1, no size (gotcha 3).
    let disk = &mslug.claims[3];
    assert_eq!(disk.kind, ClaimKind::Disk);
    assert_eq!(disk.size, None);
    assert_eq!(disk.attrs.get("index").map(String::as_str), Some("0"));
    assert_eq!(
        mslug.attrs.get("driver:status").map(String::as_str),
        Some("good")
    );
    assert_eq!(
        mslug.attrs.get("feature:graphics").map(String::as_str),
        Some("imperfect")
    );
    assert_eq!(
        mslug.attrs.get("softwarelist:neogeo").map(String::as_str),
        Some("tag=cart;status=original")
    );

    let z80 = &dat.entries[2];
    assert!(z80.is_device);
    assert!(z80.claims.is_empty());
}

#[test]
fn softlist_parts_are_lossless_and_claims_stay_flat() {
    let dat = softlist::parse(SOFTLIST.as_bytes()).expect("fixture parses");
    assert_eq!(dat.header.name.as_deref(), Some("gba"));
    assert_eq!(dat.entries.len(), 2);

    let alpha = &dat.entries[0];
    assert_eq!(alpha.manufacturer.as_deref(), Some("Ninty"));
    assert_eq!(
        alpha.attrs.get("info:serial").map(String::as_str),
        Some("AGB-AAAE-USA")
    );
    assert_eq!(
        alpha
            .attrs
            .get("sharedfeat:compatibility")
            .map(String::as_str),
        Some("GBA")
    );

    // Flat audit view: 3 rom rows + 1 disk row.
    assert_eq!(alpha.claims.len(), 4);

    // Lossless structure: parts index into the same claim rows.
    assert_eq!(alpha.parts.len(), 2);
    let cart = &alpha.parts[0];
    assert_eq!(cart.interface, "gba_cart");
    assert_eq!(cart.features.get("slot").map(String::as_str), Some("rom"));
    let area = &cart.dataareas[0];
    assert_eq!(area.size, Some(0x0080_0000)); // hex size parsed
    assert_eq!(area.width.as_deref(), Some("32"));
    assert_eq!(area.claims, [0, 1, 2]);

    let first = &alpha.claims[area.claims[0]];
    assert_eq!(first.name, "alpha.bin");
    assert_eq!(
        first.attrs.get("offset").map(String::as_str),
        Some("0x000000")
    );

    // Nameless load directives keep their loadflags.
    let cont = &alpha.claims[area.claims[1]];
    assert_eq!(cont.name, "");
    assert_eq!(
        cont.attrs.get("loadflag").map(String::as_str),
        Some("continue")
    );
    let ignore = &alpha.claims[area.claims[2]];
    assert_eq!(
        ignore.attrs.get("loadflag").map(String::as_str),
        Some("ignore")
    );
    assert_eq!(ignore.size, None);

    let diskpart = &alpha.parts[1];
    let diskarea = &diskpart.diskareas[0];
    assert_eq!(diskarea.claims, [3]);
    assert_eq!(alpha.claims[3].kind, ClaimKind::Disk);
    assert_eq!(alpha.claims[3].name, "alpha disc");

    let clone = &dat.entries[1];
    assert_eq!(clone.cloneof.as_deref(), Some("alphaadv"));
    assert_eq!(
        clone.attrs.get("supported").map(String::as_str),
        Some("partial")
    );
}

const CMPRO: &str = r#"clrmamepro (
	name "Nintendo - Nintendo Entertainment System"
	description "Nintendo NES (headered)"
	version 20260630
	header No-Intro_NES.xml
	forcemerging none
)
game (
	name "Weird (Name) (With Parens)"
	description "Weird (Name) (With Parens)"
	year 1988
	manufacturer "Some Corp"
	cloneof parent1
	rom ( name "Weird (USA).nes" size 40976 crc 1A2B3C4D )
	rom ( name broken.nes size 40976 crc DEADBEEF flags baddump )
	sample boing
)
resource (
	name bios1
	description "Console BIOS"
	rom ( name bios.rom size 8192 crc 0000ABCD sha1 3333333333333333333333333333333333333333 )
)
"#;

#[test]
fn cmpro_sections_quoting_and_flags() {
    let dat = cmpro::parse(CMPRO.as_bytes()).expect("fixture parses");
    assert_eq!(
        dat.header.name.as_deref(),
        Some("Nintendo - Nintendo Entertainment System")
    );
    assert_eq!(dat.header.detector.as_deref(), Some("No-Intro_NES.xml"));
    assert_eq!(dat.header.force_merging.as_deref(), Some("none"));

    let game = &dat.entries[0];
    // Quoted names survive embedded parens and spaces.
    assert_eq!(game.name, "Weird (Name) (With Parens)");
    assert_eq!(game.cloneof.as_deref(), Some("parent1"));
    // crc-only claim (gotcha 2) with mixed-case hex.
    let rom = &game.claims[0];
    assert_eq!(rom.name, "Weird (USA).nes");
    assert_eq!(rom.size, Some(40_976));
    assert_eq!(rom.crc32, Some([0x1a, 0x2b, 0x3c, 0x4d]));
    assert!(rom.sha1.is_none());
    assert_eq!(game.claims[1].status, ClaimStatus::BadDump);
    assert_eq!(game.claims[2].kind, ClaimKind::Sample);

    // resource sections are bios containers.
    let bios = &dat.entries[1];
    assert!(bios.is_bios);
    assert_eq!(bios.name, "bios1");
    assert!(bios.claims[0].sha1.is_some());
}

#[test]
fn dispatch_routes_and_rejects() {
    assert_eq!(parse(LOGIQX.as_bytes()).unwrap().entries.len(), 3);
    assert_eq!(parse(LISTXML.as_bytes()).unwrap().entries.len(), 3);
    assert_eq!(parse(SOFTLIST.as_bytes()).unwrap().entries.len(), 2);
    assert_eq!(parse(CMPRO.as_bytes()).unwrap().entries.len(), 2);
    assert!(matches!(
        parse(b"[CREDITS]\nauthor=x"),
        Err(ParseError::Unsupported(_))
    ));
    assert!(matches!(parse(b"NES\x1a"), Err(ParseError::UnknownFormat)));
}

#[test]
fn malformed_inputs_error_rather_than_lose_data() {
    // Malformed non-empty hash is a hard error (would weaken unification).
    let bad = LOGIQX.replace("crc=\"1b2c3d4e\"", "crc=\"nothex!!\"");
    assert!(logiqx::parse(bad.as_bytes()).is_err());
    // Unknown status is a hard error, not a silent Good.
    let bad = LOGIQX.replace("status=\"baddump\"", "status=\"greatdump\"");
    assert!(logiqx::parse(bad.as_bytes()).is_err());
    // Unterminated cmpro quote.
    assert!(cmpro::parse(b"clrmamepro ( name \"oops )").is_err());
}
