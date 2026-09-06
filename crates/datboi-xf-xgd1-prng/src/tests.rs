use super::*;

/// Game-partition sector 0 of Halo: Combat Evolved (USA) (v1.02), layout tool
/// version 3926. Its seed was recovered by exhaustive search and independently
/// confirmed to reproduce game-partition sectors 0..31 byte for byte.
const V102: &[u8; SECTOR_LEN] = include_bytes!("../data/v102_gp0.bin");
const V102_SEED: u32 = 0x4E99_8EB0;

/// Game-partition sector 0 of the same title at v1.09, layout tool version 5659.
/// This is rc4-drop-2048 filler; no seed exists in the 2^32 space.
const RC4: &[u8; SECTOR_LEN] = include_bytes!("../data/rc4_gp0.bin");

/// `(b index, stream sector, bound, alpha is that close *below* P)`; see
/// `round_trips_with_degenerate_multipliers`.
const DEGENERATE_CASES: &[(usize, u32, u32, bool)] = &[
    (0, 16_752_306, 63, false),
    (0, 1_539_686, 139, true),
    (1, 18_796_879, 86, false),
    (1, 12_567_378, 122, true),
    (2, 6_395_925, 122, false),
    (2, 16_979_693, 581, true),
    (3, 14_580_097, 65, false),
    (3, 3_769_699, 14, true),
    (4, 18_952_659, 14, false),
    (4, 30_417_311, 232, true),
    (5, 25_842_709, 103, false),
    (5, 12_478_988, 698, true),
    (6, 5_811_423, 96, false),
    (6, 32_450_395, 11, true),
    (7, 3_091_404, 31, false),
    (7, 23_783_324, 32, true),
];

fn ws() -> Box<Workspace> {
    Box::new(Workspace::new())
}

/// splitmix64, so the tests need no dependencies.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn u32(&mut self) -> u32 {
        (self.next() >> 32) as u32
    }
}

// -------------------------------------------------------------------------
// Field arithmetic
// -------------------------------------------------------------------------

#[test]
fn reduce_matches_remainder() {
    let mut r = Rng(1);
    for _ in 0..200_000 {
        let x = r.next();
        assert_eq!(Fp::reduce(x).get() as u64, x % P as u64, "x={x:#x}");
    }
    for x in [0u64, 1, P as u64 - 1, P as u64, P as u64 + 1, u64::MAX] {
        assert_eq!(Fp::reduce(x).get() as u64, x % P as u64, "x={x:#x}");
    }
}

#[test]
fn ops_preserve_the_reduced_invariant() {
    let mut r = Rng(2);
    for _ in 0..200_000 {
        let a = Fp::reduce(r.next());
        let b = Fp::reduce(r.next());
        for v in [a.add(b), a.sub(b), a.mul(b)] {
            assert!(v.get() < P);
        }
        assert_eq!(a.add(b).sub(b), a);
        if a.get() != 0 {
            assert_eq!(a.mul(a.inv()), Fp::ONE);
        }
    }
}

#[test]
fn affine_pow_matches_iteration() {
    for &bs in B_SEEDS.iter() {
        let b = Fp::reduce(bs as u64);
        let x0 = Fp::reduce(0x1234_5678);
        let mut x = x0;
        for m in 0..64u64 {
            let (alpha, beta) = affine_pow(b, m);
            assert_eq!(alpha.mul(x0).add(beta), x, "b={bs:#x} m={m}");
            x = x.mul(b).add(b); // T(x) = b*x + b
        }
    }
}

// -------------------------------------------------------------------------
// Generator
// -------------------------------------------------------------------------

/// Byte-for-byte agreement with the reference C implementation from
/// `xbox_shrinker`, for seed 0xDEADBEEF.
#[test]
fn generator_matches_reference_vector() {
    let expect: [u8; 16] = [
        0x4d, 0xbc, 0x2b, 0xdf, 0x2a, 0x40, 0x04, 0xaa, 0x2e, 0x95, 0xb8, 0x64, 0x61, 0x4e, 0x6f,
        0xa2,
    ];
    let mut out = [0u8; SECTOR_LEN];
    Prng::new(0xDEAD_BEEF, 0).fill_sector(&mut out);
    assert_eq!(&out[..16], &expect[..]);
}

#[test]
fn generator_reproduces_the_real_disc() {
    let mut out = [0u8; SECTOR_LEN];
    Prng::new(V102_SEED, 0).fill_sector(&mut out);
    assert_eq!(&out[..], &V102[..]);
    assert!(verify(V102_SEED, 0, V102));
}

/// Jumping straight to filler sector `k` must equal streaming through to it.
#[test]
fn stream_sector_jump_matches_sequential() {
    let seed = 0x0BAD_C0DE;
    let mut seq = Prng::new(seed, 0);
    let mut buf = [0u8; SECTOR_LEN];
    for k in 0..24u32 {
        seq.fill_sector(&mut buf);
        let mut jump = [0u8; SECTOR_LEN];
        Prng::new(seed, k + 1).fill_sector(&mut jump);
        let mut next = [0u8; SECTOR_LEN];
        let mut probe = seq;
        probe.fill_sector(&mut next);
        assert_eq!(jump, next, "sector {}", k + 1);
    }
}

// -------------------------------------------------------------------------
// Solver: completeness of the window
// -------------------------------------------------------------------------

/// The bucket range probed for a given `lo` must cover every value in
/// `[lo, lo + WINDOW)`. Coverage depends only on `lo & span_mask`, so this is
/// exhaustive over all inputs that can change the outcome.
#[test]
fn probe_covers_the_whole_window() {
    fn check<const LOG_SPAN: u32>() {
        let nbuckets: u32 = 1 << (24 - LOG_SPAN);
        let span_mask: u32 = (1 << LOG_SPAN) - 1;
        for off in 0..=span_mask {
            // A representative `lo` with this offset, plus the wrap-around case.
            for base in [0u32, MASK24 + 1 - (1 << LOG_SPAN)] {
                let lo = (base | off) & MASK24;
                let touched = (((lo & span_mask) + (WINDOW - 1)) >> LOG_SPAN) + 1;
                let mut covered = std::collections::BTreeSet::new();
                for t in 0..touched {
                    covered.insert((lo >> LOG_SPAN).wrapping_add(t) & (nbuckets - 1));
                }
                for d in 0..WINDOW {
                    let bv = lo.wrapping_add(d) & MASK24;
                    assert!(
                        covered.contains(&(bv >> LOG_SPAN)),
                        "LOG_SPAN={LOG_SPAN} lo={lo:#x} bv={bv:#x} not covered"
                    );
                }
            }
        }
    }
    check::<8>();
    check::<10>();
    check::<11>();
    check::<12>();
    check::<14>();
    check::<16>();
}

/// The window is a *necessary* condition: for every seed, the true `(uh, ul)`
/// pair really does land inside the probed window.
#[test]
fn true_solutions_lie_inside_the_window() {
    let mut r = Rng(7);
    for _ in 0..2000 {
        let seed = r.u32();
        let mut sector = [0u8; SECTOR_LEN];
        Prng::new(seed, 0).fill_sector(&mut sector);

        let obs0 = sector[0] as u32 | (sector[1] as u32) << 8;
        let o_lo = obs0 & 0xFF;
        let o_hi = (obs0 >> 8) & 0xFF;

        let b = Fp::reduce(B_SEEDS[(seed & 7) as usize] as u64);
        let (alpha, beta) = affine_pow(b, 1);
        let u = Fp::reduce(seed as u64).add(Fp::ONE).mul(b).get();
        let (uh, ul) = (u >> 16, u & 0xFFFF);

        let f = alpha.mul(Fp::reduce((uh as u64) << 16));
        let g = alpha.mul(Fp::reduce(ul as u64)).add(beta);

        let thi = o_hi ^ (uh & 0xFF);
        let tlo = o_lo ^ ((ul >> 8) & 0xFF);
        let a_val = f.get().wrapping_sub(thi << 16) & MASK24;
        let bv = g.get().wrapping_sub(tlo << 8) & MASK24;
        let lo = 0u32.wrapping_sub(a_val).wrapping_sub(5) & MASK24;

        assert!(
            bv.wrapping_sub(lo) & MASK24 < WINDOW,
            "seed={seed:#x} fell outside the window"
        );
    }
}

// -------------------------------------------------------------------------
// Solver: end to end
// -------------------------------------------------------------------------

#[test]
fn recovers_the_real_disc_seed() {
    assert_eq!(ws().recover_default(V102, 0), Some(V102_SEED));
}

#[test]
fn rejects_rc4_filler() {
    assert_eq!(ws().recover_default(RC4, 0), None);
}

#[test]
fn round_trips_random_seeds() {
    let mut w = ws();
    let mut r = Rng(42);
    for i in 0..512 {
        let seed = r.u32();
        let mut sector = [0u8; SECTOR_LEN];
        Prng::new(seed, 0).fill_sector(&mut sector);
        assert_eq!(w.recover_default(&sector, 0), Some(seed), "iteration {i}");
    }
}

/// Recovery must work from any filler sector, not just the first.
#[test]
fn round_trips_from_later_sectors() {
    let mut w = ws();
    let mut r = Rng(43);
    for _ in 0..64 {
        let seed = r.u32();
        let k = r.u32() % 1_000_000;
        let mut sector = [0u8; SECTOR_LEN];
        Prng::new(seed, k).fill_sector(&mut sector);
        assert_eq!(
            w.recover_default(&sector, k),
            Some(seed),
            "seed={seed:#x} k={k}"
        );
    }
}

/// Seeds at the very edges of the space, including the five that alias modulo P.
#[test]
fn round_trips_edge_seeds() {
    let mut w = ws();
    let mut edges = vec![0u32, 1, 2, 3, 4, 5, 7, 8];
    edges.extend([P - 1, P, P + 1, P + 2, P + 3, P + 4, u32::MAX]);
    edges.extend([0x8000_0000, 0x7FFF_FFFF, 0xFFFF_0000]);
    for seed in edges {
        let mut sector = [0u8; SECTOR_LEN];
        Prng::new(seed, 0).fill_sector(&mut sector);
        let got = w.recover_default(&sector, 0).expect("no seed found");
        // Aliasing seeds are acceptable so long as they regenerate the sector.
        assert!(verify(got, 0, &sector), "seed={seed:#x} got={got:#x}");
    }
}

/// Every bucket span must produce identical answers; it is a speed knob only.
#[test]
fn all_spans_agree() {
    let mut w = ws();
    let mut r = Rng(99);
    let mut cases: Vec<[u8; SECTOR_LEN]> = Vec::new();
    for _ in 0..12 {
        let mut s = [0u8; SECTOR_LEN];
        Prng::new(r.u32(), 0).fill_sector(&mut s);
        cases.push(s);
    }
    cases.push(*V102);
    cases.push(*RC4);

    for (i, s) in cases.iter().enumerate() {
        let want = w.recover::<8>(s, 0);
        assert_eq!(w.recover::<9>(s, 0), want, "span 9, case {i}");
        assert_eq!(w.recover::<10>(s, 0), want, "span 10, case {i}");
        assert_eq!(w.recover::<11>(s, 0), want, "span 11, case {i}");
        assert_eq!(w.recover::<12>(s, 0), want, "span 12, case {i}");
        assert_eq!(w.recover::<13>(s, 0), want, "span 13, case {i}");
        assert_eq!(w.recover::<14>(s, 0), want, "span 14, case {i}");
        assert_eq!(w.recover::<15>(s, 0), want, "span 15, case {i}");
        assert_eq!(w.recover::<16>(s, 0), want, "span 16, case {i}");
    }
}

/// The partitioned solver and the flat reference solver must agree exactly,
/// on solvable sectors at arbitrary stream positions and on unsolvable ones.
#[test]
fn partitioned_solver_matches_flat_reference() {
    let mut w = ws();
    let mut f = Box::new(flat::Workspace::new());
    let mut r = Rng(2024);
    for i in 0..48 {
        let mut sector = [0u8; SECTOR_LEN];
        let k = r.u32() % 4_000_000;
        let (seed, solvable) = if i % 3 == 0 {
            // Pseudorandom bytes: no seed, with overwhelming probability.
            for b in sector.iter_mut() {
                *b = r.u32() as u8;
            }
            (0, false)
        } else {
            let seed = r.u32();
            Prng::new(seed, k).fill_sector(&mut sector);
            (seed, true)
        };
        let want = f.recover_default(&sector, k);
        let got = w.recover_default(&sector, k);
        assert_eq!(got, want, "case {i} k={k}");
        if solvable {
            assert!(
                verify(got.expect("solvable"), k, &sector),
                "seed={seed:#x} k={k}"
            );
        }
    }
}

/// The largest stream sector index must not overflow the exponent arithmetic.
#[test]
fn round_trips_at_the_last_stream_sector() {
    let mut w = ws();
    for seed in [0x1234_5678u32, 0xDEAD_BEEF, 7] {
        let mut sector = [0u8; SECTOR_LEN];
        Prng::new(seed, u32::MAX).fill_sector(&mut sector);
        let got = w.recover_default(&sector, u32::MAX).expect("no seed found");
        assert!(verify(got, u32::MAX, &sector));
    }
}

/// Stream positions whose multiplier `alpha = b^(1 + 1024 k)` is tiny (or
/// tiny below `P`) pile the build-side keys into a few partitions and buckets:
/// the worst case for every capacity assumption in the partitioned join. Each
/// `(b index, k)` pair below was found by scanning `k < 2^25`; `alpha` is
/// recomputed and asserted so the test cannot silently go stale.
#[test]
fn round_trips_with_degenerate_multipliers() {
    let mut w = ws();
    for &(bi, k, alpha_bound, below_p) in DEGENERATE_CASES {
        let b = Fp::reduce(B_SEEDS[bi] as u64);
        let alpha = b.pow(1 + 1024 * k as u64).get();
        let small = if below_p { P - alpha } else { alpha };
        assert!(small <= alpha_bound, "b{bi} k={k}: alpha={alpha:#x}");
        let mut r = Rng(k as u64);
        for _ in 0..3 {
            // Seeds with the right low bits select this b.
            let seed = (r.u32() & !7) | bi as u32;
            let mut sector = [0u8; SECTOR_LEN];
            Prng::new(seed, k).fill_sector(&mut sector);
            let got = w.recover_default(&sector, k).expect("no seed found");
            assert!(
                verify(got, k, &sector),
                "b{bi} k={k} seed={seed:#x} got={got:#x}"
            );
        }
    }
}

/// A returned seed is never wrong, by construction: the solver verifies all 2048
/// bytes before returning. Corrupting the sector must therefore yield `None`.
#[test]
fn corrupted_sectors_are_rejected() {
    let mut w = ws();
    let mut r = Rng(1234);
    for _ in 0..64 {
        let seed = r.u32();
        let mut sector = [0u8; SECTOR_LEN];
        Prng::new(seed, 0).fill_sector(&mut sector);
        let at = (r.u32() as usize) % SECTOR_LEN;
        sector[at] ^= 1 << (r.u32() % 8);
        assert_eq!(
            w.recover_default(&sector, 0),
            None,
            "seed={seed:#x} at={at}"
        );
    }
}
