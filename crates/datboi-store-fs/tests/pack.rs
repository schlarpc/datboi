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
            let bytes = if ix == 0 { honest.clone() } else { liar.clone() };
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
