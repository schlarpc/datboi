//! D91 sealed packs over a real store: write → transparent resolution,
//! footer-truth across reopen, window discipline, and the
//! nothing-published failure mode.

use std::io::{Read, Seek, SeekFrom};

use datboi_core::hash::Blake3;
use datboi_store_fs::{Namespace, PackMember, Store, StoreError};

fn world() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    (dir, store)
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed))
        .collect()
}

fn members_of(pieces: &[Vec<u8>]) -> Vec<PackMember> {
    pieces
        .iter()
        .map(|p| PackMember {
            hash: Blake3::compute(p),
            len: p.len() as u64,
        })
        .collect()
}

#[test]
fn packed_members_resolve_like_loose_blobs() {
    let (dir, store) = world();
    let pieces: Vec<Vec<u8>> = vec![pattern(300, 1), pattern(70_000, 2), pattern(5, 3)];
    let members = members_of(&pieces);

    let pack = store
        .put_pack(&members, |ix| {
            Ok(Box::new(std::io::Cursor::new(pieces[ix].clone())))
        })
        .expect("pack");
    assert_eq!(store.list_packs(), vec![pack]);

    for (piece, member) in pieces.iter().zip(&members) {
        // has/len answer without a loose file existing.
        assert!(store.has(Namespace::Data, &member.hash));
        assert_eq!(
            store.len(Namespace::Data, &member.hash).expect("len"),
            Some(member.len)
        );
        // Full read round-trips.
        let mut blob = store
            .get(Namespace::Data, &member.hash)
            .expect("get")
            .expect("resolves");
        let mut bytes = Vec::new();
        blob.read_to_end(&mut bytes).expect("read");
        assert_eq!(&bytes, piece, "window serves exactly the member");
        // Seeks are window-relative and bounded: a rewind re-reads the
        // member, an End seek lands on the member's end, and reading
        // past it returns nothing (the next member's bytes are
        // unreachable by construction).
        blob.seek(SeekFrom::Start(0)).expect("rewind");
        let mut head = vec![0u8; 4.min(piece.len())];
        blob.read_exact(&mut head).expect("head");
        assert_eq!(&head, &piece[..head.len()]);
        let end = blob.seek(SeekFrom::End(0)).expect("end");
        assert_eq!(end, member.len);
        let mut past = [0u8; 8];
        assert_eq!(blob.read(&mut past).expect("read at end"), 0);
    }

    // Footers are the truth (D15): a fresh open re-scans and resolves
    // identically — no database anywhere in this test.
    drop(store);
    let store = Store::open(dir.path().join("store")).expect("reopen");
    for (piece, member) in pieces.iter().zip(&members) {
        let mut blob = store
            .get(Namespace::Data, &member.hash)
            .expect("get")
            .expect("resolves after rescan");
        let mut bytes = Vec::new();
        blob.read_to_end(&mut bytes).expect("read");
        assert_eq!(&bytes, piece);
    }

    // A loose copy wins reads (same bytes either way), and packs never
    // leak into the meta namespace.
    assert!(
        store
            .get(Namespace::Meta, &members[0].hash)
            .expect("get")
            .is_none()
    );
}

#[test]
fn member_mismatch_publishes_nothing() {
    let (_dir, store) = world();
    let honest = pattern(100, 7);
    let liar = pattern(100, 8);
    let members = vec![
        PackMember {
            hash: Blake3::compute(&honest),
            len: honest.len() as u64,
        },
        PackMember {
            hash: Blake3::compute(&honest), // claims honest…
            len: honest.len() as u64,
        },
    ];
    let err = store
        .put_pack(&members, |ix| {
            // …but streams liar bytes for member 1.
            let bytes = if ix == 0 {
                honest.clone()
            } else {
                liar.clone()
            };
            Ok(Box::new(std::io::Cursor::new(bytes)))
        })
        .expect_err("mismatch must refuse");
    assert!(matches!(err, StoreError::PackMemberMismatch { .. }));
    assert!(store.list_packs().is_empty(), "nothing published");
    assert!(
        !store.has(Namespace::Data, &members[0].hash),
        "even the honest member is unpublished — packs are all-or-nothing"
    );

    // And the empty pack is refused outright.
    assert!(matches!(
        store.put_pack(&[], |_| Ok(Box::new(std::io::empty()))),
        Err(StoreError::EmptyPack)
    ));
}

fn pack_file(root: &std::path::Path, pack: &Blake3) -> std::path::PathBuf {
    let hex = pack.to_hex();
    root.join("store")
        .join("packs")
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(hex)
}

#[test]
fn scrub_certifies_an_intact_pack_and_yields_aliases() {
    let (dir, store) = world();
    let pieces: Vec<Vec<u8>> = vec![pattern(300, 1), pattern(70_000, 2), pattern(5, 3)];
    let members = members_of(&pieces);
    let pack = store
        .put_pack(&members, |ix| {
            Ok(Box::new(std::io::Cursor::new(pieces[ix].clone())))
        })
        .expect("pack");

    let scrub = store.scrub_pack(&pack).expect("scrub");
    assert!(scrub.intact, "a freshly written pack re-hashes to its name");
    assert_eq!(scrub.members.len(), pieces.len());
    for (piece, member) in pieces.iter().zip(&scrub.members) {
        let aliases = member
            .aliases
            .as_ref()
            .expect("intact ⇒ every member verifies");
        assert_eq!(aliases.blake3, Blake3::compute(piece));
        assert_eq!(member.len, piece.len() as u64);
    }
    // Survives a reopen (the map is rebuilt from footers, scrub reads
    // bytes off disk regardless).
    drop(store);
    let store = Store::open(dir.path().join("store")).expect("reopen");
    assert!(store.scrub_pack(&pack).expect("scrub").intact);
}

#[test]
fn scrub_catches_a_rotted_member() {
    let (dir, store) = world();
    let pieces: Vec<Vec<u8>> = vec![pattern(300, 1), pattern(9000, 2), pattern(64, 3)];
    let members = members_of(&pieces);
    let pack = store
        .put_pack(&members, |ix| {
            Ok(Box::new(std::io::Cursor::new(pieces[ix].clone())))
        })
        .expect("pack");

    // Flip a byte inside member 1's data region (its window starts at
    // offset 300, past member 0). The pack map is unchanged — only the
    // bytes on disk rotted, exactly the bitrot scrub exists to catch.
    let path = pack_file(dir.path(), &pack);
    let mut bytes = std::fs::read(&path).expect("read pack");
    bytes[500] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("write pack");

    let scrub = store.scrub_pack(&pack).expect("scrub");
    assert!(!scrub.intact, "whole-file hash no longer matches the name");
    assert!(
        scrub.members[0].aliases.is_some(),
        "member 0 sits before the flip and still verifies"
    );
    assert!(
        scrub.members[1].aliases.is_none(),
        "the rotted member's slice no longer hashes to its identity"
    );
    assert!(scrub.members[2].aliases.is_some(), "member 2 untouched");
}

#[test]
fn scrub_names_a_pack_whose_footer_rotted() {
    let (dir, store) = world();
    let pieces: Vec<Vec<u8>> = vec![pattern(128, 4), pattern(256, 5)];
    let members = members_of(&pieces);
    let pack = store
        .put_pack(&members, |ix| {
            Ok(Box::new(std::io::Cursor::new(pieces[ix].clone())))
        })
        .expect("pack");

    // Corrupt the trailer magic so the footer will not locate.
    let path = pack_file(dir.path(), &pack);
    let mut bytes = std::fs::read(&path).expect("read pack");
    let n = bytes.len();
    bytes[n - 1] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("write pack");

    assert!(matches!(
        store.scrub_pack(&pack),
        Err(StoreError::PackFooter { .. })
    ));
}

#[test]
fn repack_drops_dead_members_and_keeps_survivors() {
    let (dir, store) = world();
    let pieces: Vec<Vec<u8>> = vec![
        pattern(300, 1),
        pattern(9000, 2),
        pattern(64, 3),
        pattern(500, 4),
    ];
    let members = members_of(&pieces);
    let pack = store
        .put_pack(&members, |ix| {
            Ok(Box::new(std::io::Cursor::new(pieces[ix].clone())))
        })
        .expect("pack");

    // Tombstone members 1 and 3 (the 9000- and 500-byte pieces).
    let dead: std::collections::HashSet<Blake3> =
        [members[1].hash, members[3].hash].into_iter().collect();
    let outcome = store.repack(&pack, &dead).expect("repack");
    assert_eq!(outcome.bytes_freed, 9000 + 500);
    assert_eq!(outcome.dropped.len(), 2);
    let new_pack = outcome.new_pack.expect("survivors remain");
    assert_ne!(new_pack, pack, "rewrite changes the pack identity");

    // Survivors still resolve, byte-exact, from the new pack.
    for keep in [0usize, 2] {
        assert!(store.is_packed(&members[keep].hash));
        let mut blob = store
            .get(Namespace::Data, &members[keep].hash)
            .expect("get")
            .expect("survivor resolves");
        let mut bytes = Vec::new();
        blob.read_to_end(&mut bytes).expect("read");
        assert_eq!(&bytes, &pieces[keep]);
    }
    // Dropped members are gone from the map and the old pack is unlinked.
    assert!(!store.is_packed(&members[1].hash));
    assert!(!store.is_packed(&members[3].hash));
    assert!(!pack_file(dir.path(), &pack).exists(), "old pack unlinked");
    assert_eq!(store.list_packs(), vec![new_pack]);

    // Footer-truth survives a reopen: the new pack alone resolves.
    drop(store);
    let store = Store::open(dir.path().join("store")).expect("reopen");
    assert!(store.is_packed(&members[0].hash));
    assert!(!store.is_packed(&members[1].hash));
}

#[test]
fn repack_dropping_every_member_deletes_the_pack() {
    let (dir, store) = world();
    let pieces: Vec<Vec<u8>> = vec![pattern(100, 5), pattern(200, 6)];
    let members = members_of(&pieces);
    let pack = store
        .put_pack(&members, |ix| {
            Ok(Box::new(std::io::Cursor::new(pieces[ix].clone())))
        })
        .expect("pack");

    let drop: std::collections::HashSet<Blake3> = members.iter().map(|m| m.hash).collect();
    let outcome = store.repack(&pack, &drop).expect("repack");
    assert!(outcome.new_pack.is_none(), "whole pack reclaimed");
    assert_eq!(outcome.bytes_freed, 300);
    assert!(store.list_packs().is_empty());
    assert!(!pack_file(dir.path(), &pack).exists());
    assert!(!store.has(Namespace::Data, &members[0].hash));
}

#[test]
fn repack_with_no_matching_drop_is_a_noop() {
    let (_dir, store) = world();
    let pieces: Vec<Vec<u8>> = vec![pattern(100, 7)];
    let members = members_of(&pieces);
    let pack = store
        .put_pack(&members, |ix| {
            Ok(Box::new(std::io::Cursor::new(pieces[ix].clone())))
        })
        .expect("pack");
    let drop: std::collections::HashSet<Blake3> =
        [Blake3::compute(b"not a member")].into_iter().collect();
    let outcome = store.repack(&pack, &drop).expect("repack");
    assert_eq!(outcome.new_pack, Some(pack), "pack stands unchanged");
    assert_eq!(outcome.bytes_freed, 0);
    assert!(outcome.dropped.is_empty());
    assert_eq!(store.list_packs(), vec![pack]);
}

#[test]
fn pack_files_are_content_addressed_and_deterministic() {
    let (_dir, store_a) = world();
    let (_dir_b, store_b) = world();
    let pieces: Vec<Vec<u8>> = vec![pattern(1000, 21), pattern(2000, 22)];
    let members = members_of(&pieces);
    let open = |pieces: Vec<Vec<u8>>| {
        move |ix: usize| -> std::io::Result<Box<dyn Read>> {
            Ok(Box::new(std::io::Cursor::new(pieces[ix].clone())))
        }
    };
    let a = store_a.put_pack(&members, open(pieces.clone())).expect("a");
    let b = store_b.put_pack(&members, open(pieces)).expect("b");
    assert_eq!(a, b, "same members, same order ⇒ same pack identity");
}
