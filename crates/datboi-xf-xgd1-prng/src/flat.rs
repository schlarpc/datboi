//! Reference solver: the flat (single-level) meet-in-the-middle join.
//!
//! This is the original formulation: one counting sort of all `2^16` build
//! entries into `2^(24 - LOG_SPAN)` buckets, then `2^16` random range probes.
//! Its working set is ~512 KiB, so every random access is an L2 hit. The main
//! [`crate::Workspace`] is a radix-partitioned refinement of this that keeps the
//! random accesses in L1; this version is kept as the simplest correct
//! implementation and as an oracle for the tests and benchmarks.

use crate::{B_SEEDS, Fp, MASK24, P, SECTOR_LEN, WINDOW, WORDS_PER_SECTOR, affine_pow, verify};

/// Scratch tables for the flat solver, [`Workspace::recover`].
///
/// Roughly 512 KiB. Allocate once and reuse across a whole corpus; the solver
/// re-initialises everything it reads.
pub struct Workspace {
    /// Bucket boundaries. After the fill pass, `cnt[j]` is the start of bucket `j`
    /// and `cnt[j + 1]` its end.
    cnt: [u32; 65537],
    /// Packed `(low bits of B) << 16 | ul`.
    slots: [u32; 65536],
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    /// Creates a zeroed workspace.
    pub const fn new() -> Self {
        Workspace {
            cnt: [0; 65537],
            slots: [0; 65536],
        }
    }

    /// Recovers the seed that generates `sector` as filler sector `stream_sector`.
    ///
    /// `LOG_SPAN` is the base-2 log of how many `B` values share a bucket; it trades
    /// bucket-array footprint against entries scanned per probe and must lie in
    /// `8..=16`. It affects only speed: every value of `LOG_SPAN` returns the same
    /// answer.
    ///
    /// Returns `None` if no seed in the full `2^32` space produces this sector,
    /// which is the expected outcome for the later RC4-drop-2048 filler.
    pub fn recover<const LOG_SPAN: u32>(
        &mut self,
        sector: &[u8; SECTOR_LEN],
        stream_sector: u32,
    ) -> Option<u32> {
        assert!(
            LOG_SPAN >= 8 && LOG_SPAN <= 16,
            "LOG_SPAN must be in 8..=16"
        );
        let nbuckets: usize = 1usize << (24 - LOG_SPAN);
        let bucket_mask: u32 = (nbuckets - 1) as u32;
        let span_mask: u32 = (1u32 << LOG_SPAN) - 1;

        // First two observed words of this sector.
        let obs0 = sector[0] as u32 | (sector[1] as u32) << 8;
        let obs1 = sector[2] as u32 | (sector[3] as u32) << 8;
        let o_lo = obs0 & 0xFF;
        let o_hi = (obs0 >> 8) & 0xFF;
        let o_lo1 = obs1 & 0xFF;
        let o_hi1 = (obs1 >> 8) & 0xFF;

        // A slot entry is 16 bits of `ul` plus LOG_SPAN bits of B; whatever is left
        // carries a high prefix of B2, the same build-side quantity derived from the
        // *second* observed word. Word 1 gives an independent 261-wide window, so
        // this prefix rejects candidates without a single multiply. It is a
        // necessary condition only, so completeness is preserved.
        let fbits: u32 = 16 - LOG_SPAN;
        let fshift: u32 = 24 - fbits; // >= 9, so the window spans at most 2 prefixes
        let fmask: u32 = if fbits == 0 { 0 } else { (1u32 << fbits) - 1 };
        let flow: u32 = (1u32 << fshift) - 1;
        let eshift: u32 = 16 + LOG_SPAN;

        // Word index of sector start: c_{2 + 1024*stream_sector}, i.e. T^m(u) with
        // m = 1 + 1024*stream_sector.
        let m = 1u64 + WORDS_PER_SECTOR as u64 * stream_sector as u64;

        // Borrow the tables as fixed-size arrays rather than slices: `nbuckets` is a
        // compile-time constant, so every index below is provably in range and the
        // bounds checks fold away.
        let Workspace { cnt, slots } = self;

        for i in 0..8usize {
            let b = Fp::reduce(B_SEEDS[i] as u64);
            let (alpha, beta) = affine_pow(b, m);
            // Word 1 is one more application of T(x) = b*x + b, so it is affine too.
            let alpha2 = alpha.mul(b);
            let beta2 = beta.mul(b).add(b);
            let binv = b.inv();

            // ---- build side: bucket B(ul) for every low half ----
            cnt[..=nbuckets].fill(0);

            // B(ul) = (G(ul) - 256*Tlo(ul)) mod 2^24, G(ul) = alpha*ul + beta.
            // Tlo depends only on bits 8..15 of ul, so it is hoisted one level out.
            let mut g = beta;
            for uhi in 0..256u32 {
                let tlo = (o_lo ^ uhi) << 8;
                for _ in 0..256u32 {
                    let bv = g.get().wrapping_sub(tlo) & MASK24;
                    cnt[(bv >> LOG_SPAN) as usize] += 1;
                    g = g.add(alpha);
                }
            }
            for j in 1..nbuckets {
                cnt[j] += cnt[j - 1];
            }
            cnt[nbuckets] = 65536;

            let mut g = beta;
            let mut g2 = beta2;
            for uhi in 0..256u32 {
                let tlo = (o_lo ^ uhi) << 8;
                let tlo1 = (o_lo1 ^ uhi) << 8;
                let ul_base = uhi << 8;
                for ulo in 0..256u32 {
                    let bv = g.get().wrapping_sub(tlo) & MASK24;
                    let bv2 = g2.get().wrapping_sub(tlo1) & MASK24;
                    let fpart = if fbits == 0 {
                        0
                    } else {
                        (bv2 >> fshift) << eshift
                    };
                    let j = (bv >> LOG_SPAN) as usize;
                    cnt[j] -= 1;
                    slots[(cnt[j] & 0xFFFF) as usize] =
                        fpart | ((bv & span_mask) << 16) | (ul_base | ulo);
                    g = g.add(alpha);
                    g2 = g2.add(alpha2);
                }
            }
            // cnt[j] is now the start of bucket j, cnt[j+1] its end.

            // ---- probe side: for every high half, range-query the window ----
            let mut f = Fp::ZERO;
            let mut f2 = Fp::ZERO;
            let f_step = alpha.mul(Fp::reduce(65536));
            let f2_step = alpha2.mul(Fp::reduce(65536));
            for uh in 0..65536u32 {
                let ub = uh & 0xFF;
                let a_val = f.get().wrapping_sub((o_hi ^ ub) << 16) & MASK24;
                let a2_val = f2.get().wrapping_sub((o_hi1 ^ ub) << 16) & MASK24;
                f = f.add(f_step);
                f2 = f2.add(f2_step);

                // Solutions satisfy B ∈ [lo, lo + WINDOW) mod 2^24.
                let lo = 0u32.wrapping_sub(a_val).wrapping_sub(5) & MASK24;
                let touched = (((lo & span_mask) + (WINDOW - 1)) >> LOG_SPAN) + 1;

                // ...and the same for word 1, used only through its stored prefix.
                let lo2 = 0u32.wrapping_sub(a2_val).wrapping_sub(5) & MASK24;
                let tf = lo2 >> fshift;
                let nfv = (((lo2 & flow) + (WINDOW - 1)) >> fshift) + 1;

                for t in 0..touched {
                    let j = ((lo >> LOG_SPAN).wrapping_add(t) & bucket_mask) as usize;
                    let start = cnt[j] as usize;
                    let end = cnt[j + 1] as usize;
                    for s in start..end {
                        let e = slots[s & 0xFFFF];
                        let bv = ((j as u32) << LOG_SPAN) | ((e >> 16) & span_mask);
                        if bv.wrapping_sub(lo) & MASK24 >= WINDOW {
                            continue;
                        }
                        // Multiply-free rejection on the second word.
                        if fbits != 0 && (e >> eshift).wrapping_sub(tf) & fmask >= nfv {
                            continue;
                        }
                        let ul = e & 0xFFFF;
                        let u = (uh << 16) | ul;
                        if u >= P {
                            continue;
                        }
                        // Exact check on word 0, then word 1, then full verification.
                        let ufp = Fp::reduce(u as u64);
                        let c = alpha.mul(ufp).add(beta);
                        if ((c.get() >> 8) & 0xFFFF) != (obs0 ^ ((u >> 8) & 0xFFFF)) {
                            continue;
                        }
                        let c2 = c.add(Fp::ONE).mul(b);
                        if ((c2.get() >> 8) & 0xFFFF) != (obs1 ^ ((u >> 8) & 0xFFFF)) {
                            continue;
                        }
                        // u = b*(seed + 1)  =>  seed = u*b^-1 - 1.
                        let s0 = ufp.mul(binv).sub(Fp::ONE).get();
                        // Seeds >= P alias onto the same u with a different b, so
                        // both representatives must be considered.
                        for cand in [s0, s0.wrapping_add(P)] {
                            if cand < s0 {
                                continue; // wrapped past 2^32: not a distinct seed
                            }
                            if (cand & 7) as usize != i {
                                continue;
                            }
                            if verify(cand, stream_sector, sector) {
                                return Some(cand);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Convenience wrapper using this solver's best bucket span, 8.
    #[inline]
    pub fn recover_default(
        &mut self,
        sector: &[u8; SECTOR_LEN],
        stream_sector: u32,
    ) -> Option<u32> {
        self.recover::<8>(sector, stream_sector)
    }
}
