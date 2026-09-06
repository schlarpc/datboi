//! Recovery of the 32-bit seed for the proprietary Xbox XGD1 filler PRNG.
//!
//! # The generator
//!
//! Early XGD1 discs (mastering-tool version <= 4830) fill the unused sectors of the
//! game partition with a stream from a proprietary Microsoft PRNG over the prime
//! field `GF(2^32 - 5)`:
//!
//! ```text
//! b     = B_SEEDS[seed & 7]
//! c_1   = b * (seed + 1)                 (mod P)
//! c_n+1 = b * (c_n + 1)                  (mod P)
//! a     = c_1                            (fixed after seeding)
//! word  = bits 8..23 of (c_n XOR a),  n >= 2, emitted little-endian
//! ```
//!
//! Each 2048-byte sector consumes exactly 1024 words. The generator advances *only*
//! over filler sectors: real file/metadata extents do not consume the stream.
//!
//! # Recovery
//!
//! `c_n` is affine in the unknown `u = c_1`, so with `u = uh*2^16 + ul`:
//!
//! ```text
//! c_n = F(uh) + G(ul)   (mod P),   F(uh) = alpha*uh*2^16,  G(ul) = alpha*ul + beta
//! ```
//!
//! The observation is `obs = M(c_n) XOR M(u)` where `M(x) = bits 8..23 of x`. The
//! XOR mask splits cleanly because bits 8..15 of `u` come only from `ul` and bits
//! 16..23 come only from `uh`. Using `2^32 ≡ 5 (mod P)`, the modular wrap collapses
//! to a `+5w` term modulo `2^24`, giving a separable condition:
//!
//! ```text
//! A(uh) + B(ul) + 5w  ∈  [0, 256)   (mod 2^24),   w ∈ {0, 1}
//! ```
//!
//! so every solution has `B(ul)` inside a 261-wide window determined by `uh`:
//! a join of `2^16` keys against `2^16` windows, `~2^17` work against `2^32` for
//! exhaustive search. Word 1 of the sector gives a second, independent window
//! that filters the join's candidates before any exact arithmetic.
//!
//! The join is radix-partitioned so that it runs out of L1, and the partitioning
//! is free: within a row of 256 consecutive `ul` (or `uh`) the keys are an
//! arithmetic progression, so one sorted 256-entry table per `b` yields every
//! partition's members as a contiguous run of a cyclic walk. See
//! [`Workspace::recover`].
//!
//! # Correctness
//!
//! * **Soundness is structural.** [`Workspace::recover`] only ever returns a seed
//!   that has been re-verified by regenerating and comparing all 2048 bytes. No
//!   step of the meet-in-the-middle is trusted for correctness.
//! * **Completeness** rests on the window above being a *necessary* condition for a
//!   solution, and on the join visiting every `(uh, ul)` pair that satisfies it.
//!   Every approximation in the join (keys without their carry term, walks that
//!   start early and end late, prefix tests wider than the window) errs on the
//!   side of visiting more pairs, never fewer. This is tested exhaustively where
//!   possible and differentially against the flat reference solver in [`flat`].
//! * [`Fp`] values are reduced by construction; the modulus is never divided by.

#![deny(unsafe_code)]
#![deny(missing_docs)]

/// Field modulus `2^32 - 5`, the largest prime below `2^32`.
pub const P: u32 = 0xFFFF_FFFB;

/// The eight `b` multipliers, selected by the low three bits of the seed.
pub const B_SEEDS: [u32; 8] = [
    0x52F6_90D5,
    0x534D_7DDE,
    0x5B71_A70F,
    0x6679_3320,
    0x9B7E_5ED5,
    0xA465_265E,
    0xA53F_1D11,
    0xB154_430F,
];

/// Bytes per disc sector.
pub const SECTOR_LEN: usize = 2048;

/// PRNG words consumed per filler sector (two bytes each).
pub const WORDS_PER_SECTOR: u32 = (SECTOR_LEN / 2) as u32;

// ---------------------------------------------------------------------------
// Field arithmetic
// ---------------------------------------------------------------------------

/// An element of `GF(2^32 - 5)`, reduced by construction.
///
/// The inner value is private and every constructor and operation re-establishes
/// the invariant `0 <= value < P`, so a `Fp` can never hold an unreduced number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fp(u32);

impl Fp {
    /// The additive identity.
    pub const ZERO: Self = Fp(0);
    /// The multiplicative identity.
    pub const ONE: Self = Fp(1);

    /// Reduces an arbitrary 64-bit value into the field.
    ///
    /// Uses `2^32 ≡ 5 (mod P)` to fold the high half, which drops the residue below
    /// `6 * 2^32`; the second fold then fits entirely in 32 bits. No division or
    /// remainder instruction is involved, and only the first fold touches 64-bit
    /// arithmetic, which matters on wasm32 where `i64` ops cost more than `i32`.
    #[inline(always)]
    pub const fn reduce(x: u64) -> Self {
        let t = (x >> 32) * 5 + (x & 0xFFFF_FFFF); // < 6 * 2^32
        let lo = t as u32;
        let hi = (t >> 32) as u32; // < 6
        // true value is lo + hi*5 < 2^32 + 25
        let (s, carry) = lo.overflowing_add(hi * 5);
        Fp(if carry {
            // true = s + 2^32, and true - P = s + 5, with s < 25
            s + 5
        } else if s >= P {
            s - P
        } else {
            s
        })
    }

    /// Returns the reduced representative.
    #[inline(always)]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Field addition.
    #[inline(always)]
    pub const fn add(self, o: Self) -> Self {
        // Both inputs are < P, so the true sum is < 2P < 2^33.
        let (s, carry) = self.0.overflowing_add(o.0);
        Fp(if carry {
            // true = s + 2^32, so true - P = s + 5 (no overflow: s < 2^32 - 10)
            s + 5
        } else if s >= P {
            s - P
        } else {
            s
        })
    }

    /// Field subtraction.
    #[inline(always)]
    pub const fn sub(self, o: Self) -> Self {
        Fp(if self.0 >= o.0 {
            self.0 - o.0
        } else {
            P - (o.0 - self.0)
        })
    }

    /// Field multiplication.
    #[inline(always)]
    pub const fn mul(self, o: Self) -> Self {
        Self::reduce(self.0 as u64 * o.0 as u64)
    }

    /// Exponentiation by squaring.
    pub const fn pow(self, mut e: u64) -> Self {
        let mut base = self;
        let mut acc = Fp::ONE;
        while e != 0 {
            if e & 1 == 1 {
                acc = acc.mul(base);
            }
            base = base.mul(base);
            e >>= 1;
        }
        acc
    }

    /// Multiplicative inverse via Fermat's little theorem. Returns [`Fp::ZERO`] for zero.
    pub const fn inv(self) -> Self {
        self.pow((P - 2) as u64)
    }
}

/// Coefficients of the `m`-fold composition of `T(x) = b*x + b`.
///
/// `T^m(x) = alpha*x + beta` with `alpha = b^m` and `beta = b*(b^m - 1)/(b - 1)`.
/// Requires `b != 1`, which holds for every entry of [`B_SEEDS`].
#[inline]
pub(crate) fn affine_pow(b: Fp, m: u64) -> (Fp, Fp) {
    let alpha = b.pow(m);
    let beta = b.mul(alpha.sub(Fp::ONE)).mul(b.sub(Fp::ONE).inv());
    (alpha, beta)
}

// ---------------------------------------------------------------------------
// Reference generator
// ---------------------------------------------------------------------------

/// The filler stream generator, positioned at some point in the stream.
#[derive(Clone, Copy, Debug)]
pub struct Prng {
    a: u32,
    b: Fp,
    c: Fp,
}

impl Prng {
    /// Seeds the generator and positions it at the start of filler sector `stream_sector`.
    ///
    /// `stream_sector` counts filler sectors only: real file and metadata extents do
    /// not advance the stream.
    pub fn new(seed: u32, stream_sector: u32) -> Self {
        let b = Fp::reduce(B_SEEDS[(seed & 7) as usize] as u64);
        // c_1 = b * (seed + 1); the mask value `a` is fixed to c_1 forever after.
        let u = Fp::reduce(seed as u64).add(Fp::ONE).mul(b);
        let a = u.get();
        // Skip 1024 words per preceding filler sector.
        let (alpha, beta) = affine_pow(b, WORDS_PER_SECTOR as u64 * stream_sector as u64);
        let c = alpha.mul(u).add(beta);
        Prng { a, b, c }
    }

    /// Advances the generator and returns the next 16-bit output word.
    #[inline(always)]
    pub fn next_word(&mut self) -> u16 {
        self.c = self.c.add(Fp::ONE).mul(self.b);
        (((self.c.get() ^ self.a) >> 8) & 0xFFFF) as u16
    }

    /// Fills one sector of filler bytes.
    pub fn fill_sector(&mut self, out: &mut [u8; SECTOR_LEN]) {
        for pair in out.chunks_exact_mut(2) {
            let w = self.next_word();
            pair[0] = (w & 0xFF) as u8;
            pair[1] = (w >> 8) as u8;
        }
    }
}

/// Regenerates filler sector `stream_sector` from `seed` and compares it to `sector`.
///
/// This is the sole authority on whether a seed is correct.
pub fn verify(seed: u32, stream_sector: u32, sector: &[u8; SECTOR_LEN]) -> bool {
    let mut g = Prng::new(seed, stream_sector);
    for pair in sector.chunks_exact(2) {
        let w = g.next_word();
        if pair[0] != (w & 0xFF) as u8 || pair[1] != (w >> 8) as u8 {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Meet-in-the-middle solver
// ---------------------------------------------------------------------------

/// Width of the `B` window that must contain every solution, in `2^24` units.
///
/// The exact condition is `A + B + 5w ∈ [0, 256)` for the correct `w ∈ {0, 1}`;
/// taking the union over both `w` gives `[-5, 256)`, a window of 261 values.
pub(crate) const WINDOW: u32 = 261;

pub(crate) const MASK24: u32 = 0x00FF_FFFF;

/// Base-2 log of the number of radix partitions of the `2^24` key space.
///
/// Each partition joins ~`2^16 / 2^LOG_PART` build entries against as many
/// probes; its working set (bucket starts, sorted slots, the column tables)
/// should be small enough for random access to be cheap, against the fixed
/// cost of restarting `2 * 256` walks per partition. Measured best at 4 on both
/// x86-64 and Cranelift. It only affects speed.
const LOG_PART: u32 = 4;
const NPART: usize = 1 << LOG_PART;
/// Bit position of the partition index within a 24-bit key.
const PART_SHIFT: u32 = 24 - LOG_PART;
const MIN_LOG_SPAN: u32 = 5;
/// Largest per-partition bucket count, reached at the smallest `LOG_SPAN`.
const MAX_NB: usize = 1 << (24 - LOG_PART - MIN_LOG_SPAN);
/// Guard entries on either side of the bucket-start table, so that a probe's
/// bucket range never needs clamping: starts below bucket 0 read `0`, ends
/// past the last bucket read the entry total.
const CPAD: usize = 16;

/// Both sides are bucketed and probed by an *approximate* key that differs from
/// the exact one by a carry term of at most 5 (see [`Workspace::recover`]); the
/// walks and the probe range are widened by this much to compensate.
const KEY_SLACK: i32 = 8;

/// Slot capacity: one entry per `ul`, plus padding read by the speculative scan.
const SLOTS_LEN: usize = 65536 + 4;

/// Length of the sorted-run tables: two laps of the cyclic order, then
/// sentinels; a power of two so that walk indices can be masked, not checked.
const RUN_LEN: usize = 1024;

/// Scratch tables for [`Workspace::recover`], about 410 KiB.
///
/// Allocate once and reuse across a whole corpus; the solver re-initialises
/// everything it reads. `slots` and `cnt` are sized for the degenerate case
/// where every entry lands in one partition, or the smallest `LOG_SPAN`; the
/// working set of a normal partition is a fraction of that.
pub struct Workspace {
    /// Per-partition counting sort output: packed `(B2 >> 8) << 16 | ul`, plus
    /// a few entries of padding so the speculative scan needs no bounds checks.
    slots: [u32; SLOTS_LEN],
    /// Per-partition bucket starts, `CPAD` guard entries at each end. Only
    /// `nb + 2 * CPAD` entries are used; the array is a power of two so that
    /// indices can be masked instead of checked. `u16` holds `65536` as `0`,
    /// which the length arithmetic tolerates.
    cnt: [u16; 2 * MAX_NB],
    /// Row part of `G(ul) = alpha*ul + beta`: `gh[y] = alpha*256*y + beta`.
    gh: [u32; 256],
    /// Row part of the word-1 map `G2(ul) = alpha2*ul + beta2`.
    g2h: [u32; 256],
    /// Column part of `G2`, `(alpha2*x mod 2^24) << 8`.
    g2ls: [u32; 256],
    /// Row part of `F(uh) = alpha*uh*2^16`: `fl[ub] = alpha*2^16*ub`.
    fl: [u32; 256],
    /// Row part of the word-1 map `F2(uh) = alpha2*uh*2^16`.
    f2l: [u32; 256],
    /// Column part of `-F2`, `(-alpha2*2^24*ua mod 2^24) << 8`.
    n2s: [u32; 256],
    /// `alpha*x mod 2^24` in sorted order, two laps (`+2^24` on the second) and
    /// sentinels, with the matching `x = ul & 0xFF` alongside.
    glv: [u32; RUN_LEN],
    glx: [u8; RUN_LEN],
    /// `-alpha*2^24*ua mod 2^24` likewise, with the matching `ua = uh >> 8`.
    fhv: [u32; RUN_LEN],
    fhx: [u8; RUN_LEN],
    /// Per-row walk cursors into `glv` / `fhv`, and where the build-side walk
    /// of the current partition ended.
    gcur: [u16; 256],
    gend: [u16; 256],
    fcur: [u16; 256],
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

/// First index in `v[..256]` whose value is `>= x`, or 256.
#[inline]
fn lower_bound(v: &[u32; RUN_LEN], x: u32) -> usize {
    let mut lo = 0usize;
    let mut len = 256usize;
    while len > 0 {
        let half = len / 2;
        if v[lo + half] < x {
            lo += half + 1;
            len -= half + 1;
        } else {
            len = half;
        }
    }
    lo
}

/// Sorts 256 `(value, index)` pairs by value and lays them out as a two-lap
/// run table with sentinels.
fn build_run(keys: &mut [u32; 256], v: &mut [u32; RUN_LEN], x: &mut [u8; RUN_LEN]) {
    keys.sort_unstable();
    for i in 0..256 {
        let k = keys[i];
        v[i] = k >> 8;
        v[i + 256] = (k >> 8) + (1 << 24);
        x[i] = k as u8;
        x[i + 256] = k as u8;
    }
    for i in 512..RUN_LEN {
        v[i] = 0x3FFF_FFFF;
        x[i] = 0;
    }
}

/// Everything the exact candidate check needs, kept out of the probe loop.
struct Exact<'a> {
    alpha: Fp,
    beta: Fp,
    b: Fp,
    binv: Fp,
    obs0: u32,
    obs1: u32,
    bi: usize,
    stream_sector: u32,
    sector: &'a [u8; SECTOR_LEN],
}

/// Walks `len` slots from `start` for probe `uh`, applying the word-1 prefix
/// test and then the exact checks. Cold: the probe loop only comes here once
/// a slot has passed the prefix test, ~3 in 65536 of the ones it sees.
#[cold]
#[inline(never)]
fn scan_exact(
    x: &Exact<'_>,
    slots: &[u32; SLOTS_LEN],
    start: u16,
    len: u16,
    tf16: u32,
    uh: u32,
) -> Option<u32> {
    for k in 0..len {
        let e = slots[start.wrapping_add(k) as usize];
        if e.wrapping_sub(tf16) >= NFV16 {
            continue;
        }
        let ul = e & 0xFFFF;
        let u = (uh << 16) | ul;
        if u >= P {
            continue;
        }
        // Exact check on word 0, then word 1, then full verification.
        let ufp = Fp::reduce(u as u64);
        let c = x.alpha.mul(ufp).add(x.beta);
        if ((c.get() >> 8) & 0xFFFF) != (x.obs0 ^ ((u >> 8) & 0xFFFF)) {
            continue;
        }
        let c2 = c.add(Fp::ONE).mul(x.b);
        if ((c2.get() >> 8) & 0xFFFF) != (x.obs1 ^ ((u >> 8) & 0xFFFF)) {
            continue;
        }
        // u = b*(seed + 1)  =>  seed = u*b^-1 - 1.
        let s0 = ufp.mul(x.binv).sub(Fp::ONE).get();
        // Seeds >= P alias onto the same u with a different b, so both
        // representatives must be considered.
        for cand in [s0, s0.wrapping_add(P)] {
            if cand < s0 {
                continue; // wrapped past 2^32: not a distinct seed
            }
            if (cand & 7) as usize != x.bi {
                continue;
            }
            if verify(cand, x.stream_sector, x.sector) {
                return Some(cand);
            }
        }
    }
    None
}

/// The word-1 prefix test accepts three consecutive `B2 >> 8` values, which
/// covers the 261-wide window plus the jitter of both approximate keys.
const NFV16: u32 = 3 << 16;

impl Workspace {
    /// Creates a zeroed workspace.
    pub const fn new() -> Self {
        Workspace {
            slots: [0; SLOTS_LEN],
            cnt: [0; 2 * MAX_NB],
            gh: [0; 256],
            g2h: [0; 256],
            g2ls: [0; 256],
            fl: [0; 256],
            f2l: [0; 256],
            n2s: [0; 256],
            glv: [0; RUN_LEN],
            glx: [0; RUN_LEN],
            fhv: [0; RUN_LEN],
            fhx: [0; RUN_LEN],
            gcur: [0; 256],
            gend: [0; 256],
            fcur: [0; 256],
        }
    }

    /// Fills the row/column tables for the affine maps `(alpha, beta)` of word 0
    /// and `(alpha2, beta2)` of word 1, and the sorted run tables of word 0.
    fn fill_tables(&mut self, alpha: Fp, beta: Fp, alpha2: Fp, beta2: Fp) {
        let a256 = alpha.mul(Fp::reduce(256));
        let a2_256 = alpha2.mul(Fp::reduce(256));
        let a16 = alpha.mul(Fp::reduce(1 << 16));
        let a2_16 = alpha2.mul(Fp::reduce(1 << 16));
        let a24 = alpha.mul(Fp::reduce(1 << 24));
        let a2_24 = alpha2.mul(Fp::reduce(1 << 24));
        let (mut gh, mut gl, mut g2h, mut g2l) = (beta, Fp::ZERO, beta2, Fp::ZERO);
        let (mut fl, mut fh, mut f2l, mut f2h) = (Fp::ZERO, Fp::ZERO, Fp::ZERO, Fp::ZERO);
        let mut gkeys = [0u32; 256];
        let mut fkeys = [0u32; 256];
        for i in 0..256 {
            self.gh[i] = gh.get();
            self.g2h[i] = g2h.get();
            self.g2ls[i] = g2l.get() << 8;
            self.fl[i] = fl.get();
            self.f2l[i] = f2l.get();
            self.n2s[i] = 0u32.wrapping_sub(f2h.get()) << 8;
            gkeys[i] = ((gl.get() & MASK24) << 8) | i as u32;
            fkeys[i] = ((0u32.wrapping_sub(fh.get()) & MASK24) << 8) | i as u32;
            gh = gh.add(a256);
            gl = gl.add(alpha);
            g2h = g2h.add(a2_256);
            g2l = g2l.add(alpha2);
            fl = fl.add(a16);
            fh = fh.add(a24);
            f2l = f2l.add(a2_16);
            f2h = f2h.add(a2_24);
        }
        build_run(&mut gkeys, &mut self.glv, &mut self.glx);
        build_run(&mut fkeys, &mut self.fhv, &mut self.fhx);
    }

    /// Recovers the seed that generates `sector` as filler sector `stream_sector`.
    ///
    /// `LOG_SPAN` is the base-2 log of how many key values share a bucket; it
    /// trades bucket-table footprint against entries scanned per probe and must
    /// lie in `5..=16`. It affects only speed: every value of `LOG_SPAN` returns
    /// the same answer.
    ///
    /// Returns `None` if no seed in the full `2^32` space produces this sector,
    /// which is the expected outcome for the later RC4-drop-2048 filler.
    ///
    /// # Algorithm
    ///
    /// A radix-partitioned join whose partitioning costs nothing. Write
    /// `ul = y*256 + x` and `uh = ua*256 + ub`. Both keys split into a per-row
    /// constant plus a 256-entry table:
    ///
    /// ```text
    /// B(ul)  = D(y)  + gl(x)  + 5c   (mod 2^24)     c ∈ {0, 1}
    /// lo(uh) = C(ub) - fh(ua) - 5c'  (mod 2^24)     c' ∈ {0, 1}
    /// ```
    ///
    /// where `c`, `c'` are the modular-wrap carries. So with `gl` and `-fh`
    /// sorted once per `b` (256 entries each), every row's keys appear in sorted
    /// order as a cyclic walk over that table, up to a jitter of 5. The entries
    /// and probes of key partition `q` are therefore, for each row, a contiguous
    /// run of that walk, found by advancing a per-row cursor: no histogram, no
    /// scatter, no partitioned copies of anything.
    ///
    /// Each partition is then joined on its own in L1: a counting sort of its
    /// ~`2^16 / 2^LOG_PART` entries into `2^(24 - LOG_PART - LOG_SPAN)` buckets,
    /// then one contiguous range scan per probe. Both use the carry-free
    /// approximate key; the probe range is widened by the jitter, so this loses
    /// nothing. The scan does not test the exact word-0 window: the 16-bit
    /// word-1 prefix carried in every slot rejects all but ~3 in 65536 of the
    /// entries it sees, so the exact checks are reached a handful of times per
    /// sweep. Because a probe sees ~1.5 entries on average, the scan speculates
    /// on the first three slots branch-free and only loops when the range is
    /// longer, leaving the loop for the exact check to a prefix hit.
    ///
    /// Completeness: the walks over-approximate (each cursor starts `KEY_SLACK`
    /// early and ends late, plus the window width on the probe side), so every
    /// `(uh, ul)` pair whose keys satisfy the window condition meets in some
    /// partition. Entries that land in a partition they do not belong to are
    /// filed at a masked bucket and can only ever produce a candidate that the
    /// exact checks discard.
    pub fn recover<const LOG_SPAN: u32>(
        &mut self,
        sector: &[u8; SECTOR_LEN],
        stream_sector: u32,
    ) -> Option<u32> {
        assert!(
            LOG_SPAN >= MIN_LOG_SPAN && LOG_SPAN <= 16,
            "LOG_SPAN must be in 5..=16"
        );
        // Buckets per partition; `cnt` addresses them with 16-bit arithmetic.
        const _: () = assert!(LOG_PART + MIN_LOG_SPAN >= 8);
        let nb: usize = 1usize << (24 - LOG_PART - LOG_SPAN);
        let nb_mask: u32 = (nb - 1) as u32;
        const CMASK: usize = 2 * MAX_NB - 1;

        // First two observed words of this sector.
        let obs0 = sector[0] as u32 | (sector[1] as u32) << 8;
        let obs1 = sector[2] as u32 | (sector[3] as u32) << 8;
        let o_lo = obs0 & 0xFF;
        let o_hi = (obs0 >> 8) & 0xFF;
        let o_lo1 = obs1 & 0xFF;
        let o_hi1 = (obs1 >> 8) & 0xFF;

        // Word index of sector start: c_{2 + 1024*stream_sector}, i.e. T^m(u) with
        // m = 1 + 1024*stream_sector.
        let m = 1u64 + WORDS_PER_SECTOR as u64 * stream_sector as u64;

        for bi in 0..8usize {
            let b = Fp::reduce(B_SEEDS[bi] as u64);
            let (alpha, beta) = affine_pow(b, m);
            // Word 1 is one more application of T(x) = b*x + b, so it is affine too.
            let alpha2 = alpha.mul(b);
            let beta2 = beta.mul(b).add(b);
            let binv = b.inv();
            self.fill_tables(alpha, beta, alpha2, beta2);
            let exact = Exact {
                alpha,
                beta,
                b,
                binv,
                obs0,
                obs1,
                bi,
                stream_sector,
                sector,
            };

            let Workspace {
                slots,
                cnt,
                gh,
                g2h,
                g2ls,
                fl,
                f2l,
                n2s,
                glv,
                glx,
                fhv,
                fhx,
                gcur,
                gend,
                fcur,
            } = self;

            // Row constants of the sorted walks: B(ul) = D(y) + gl(x) + 5c with
            // D(y) = G-row - 256*Tlo(y) - 5, and lo(uh) = C(ub) + (-fh(ua)) - 5c'
            // with C(ub) = 2^16*Thi(ub) - F-row, both mod 2^24.
            let row_d = |y: usize| -> i32 {
                let tlo = (o_lo ^ y as u32) << 8;
                (gh[y].wrapping_sub(tlo).wrapping_sub(5) & MASK24) as i32
            };
            let row_c = |ub: usize| -> i32 {
                let thi = (o_hi ^ ub as u32) << 16;
                (thi.wrapping_sub(fl[ub]) & MASK24) as i32
            };
            // Start every walk KEY_SLACK (build) / KEY_SLACK + WINDOW (probe)
            // keys before 0 in the cyclic order.
            const BUILD_LEAD: i32 = KEY_SLACK;
            const PROBE_LEAD: i32 = KEY_SLACK + WINDOW as i32;
            for y in 0..256 {
                let x0 = (1 << 24) - BUILD_LEAD - row_d(y);
                gcur[y] = if x0 <= 0 {
                    0
                } else {
                    lower_bound(glv, x0 as u32) as u16
                };
                let x0 = (1 << 24) - PROBE_LEAD - row_c(y);
                fcur[y] = if x0 <= 0 {
                    0
                } else {
                    lower_bound(fhv, x0 as u32) as u16
                };
            }

            for q in 0..NPART {
                let qbase = (q as i32) << PART_SHIFT;
                let hi_b = qbase + (1 << PART_SHIFT) + KEY_SLACK;
                let next_b = qbase + (1 << PART_SHIFT) - BUILD_LEAD;
                let hi_p = hi_b;
                let next_p = qbase + (1 << PART_SHIFT) - PROBE_LEAD;

                // ---- histogram this partition's entries by bucket ----
                // The bucket is taken from the approximate key, relative to the
                // partition base; strays from the walk slack wrap onto a masked
                // bucket, which is harmless.
                cnt[..nb + 2 * CPAD].fill(0);
                for y in 0..256usize {
                    let dy = row_d(y) - (1 << 24) - qbase;
                    let hi = hi_b - qbase;
                    let mut i = gcur[y] as usize;
                    loop {
                        let k = dy + glv[i & (RUN_LEN - 1)] as i32;
                        if k >= hi {
                            break;
                        }
                        let j = (k >> LOG_SPAN) as u32 & nb_mask;
                        let c = &mut cnt[(j as usize + CPAD + 2) & CMASK];
                        *c = c.wrapping_add(1);
                        i += 1;
                    }
                    gend[y] = i as u16;
                }

                // ---- counting sort: prefix sum, then scatter ----
                // After this, cnt[CPAD + j] is the start of bucket j and
                // cnt[CPAD + j + 1] its end; the guards above nb hold the total.
                for k in CPAD + 2..CPAD + nb + 2 {
                    cnt[k] = cnt[k].wrapping_add(cnt[k - 1]);
                }
                let total = cnt[CPAD + nb + 1];
                for k in CPAD + nb + 2..nb + 2 * CPAD {
                    cnt[k] = total;
                }
                // Re-walk the same runs, now with a known length. The slot
                // carries the approximate B2 >> 8 (exact is 0..=5 above it) in
                // its top half: B2 = D2(y) + g2l(x) + 5c, pre-shifted.
                for y in 0..256usize {
                    let dy = row_d(y) - (1 << 24) - qbase;
                    let tlo1 = (o_lo1 ^ y as u32) << 8;
                    let d2s = g2h[y].wrapping_sub(tlo1) << 8;
                    let ul_row = (y << 8) as u32;
                    let (i0, i1) = (gcur[y] as usize, gend[y] as usize);
                    for i in i0..i1 {
                        let ii = i & (RUN_LEN - 1);
                        let k = dy + glv[ii] as i32;
                        let j = ((k >> LOG_SPAN) as u32 & nb_mask) as usize;
                        let x = glx[ii] as usize;
                        let b2s = d2s.wrapping_add(g2ls[x]) & 0xFFFF_0000;
                        let s = cnt[(j + CPAD + 1) & CMASK];
                        cnt[(j + CPAD + 1) & CMASK] = s.wrapping_add(1);
                        slots[s as usize] = b2s | ul_row | x as u32;
                    }
                    // Next partition's walk starts where this one's keys reach
                    // its lead-in.
                    let mut i = i1;
                    while i > 0 && dy + glv[(i - 1) & (RUN_LEN - 1)] as i32 >= next_b - qbase {
                        i -= 1;
                    }
                    gcur[y] = i as u16;
                }

                // ---- probe every uh whose window touches this partition ----
                let slots: &[u32; SLOTS_LEN] = slots;
                for ub in 0..256usize {
                    let cub = row_c(ub) - (1 << 24) - qbase;
                    let hi = hi_p - qbase;
                    // Word-1 window start, approximate (exact is 0..=5 below) and
                    // moved down by 10 to cover both sides' jitter, pre-shifted:
                    // lo2 = C2(ub) + (-f2h(ua)) - 5c''.
                    let thi1 = (o_hi1 ^ ub as u32) << 16;
                    let c2s = thi1.wrapping_sub(f2l[ub]).wrapping_sub(10) << 8;
                    let mut i = fcur[ub] as usize;
                    loop {
                        let ii = i & (RUN_LEN - 1);
                        // Approximate window start relative to the partition base;
                        // the exact one is 0..=5 below it.
                        let d = cub + fhv[ii] as i32;
                        if d >= hi {
                            break;
                        }
                        let ua = fhx[ii] as usize;
                        i += 1;

                        // Exact window [lo, lo + WINDOW) ⊂ [d - 5, d + WINDOW), and
                        // entries were bucketed by a key 0..=5 below their exact one.
                        let j0 = (d - 10) >> LOG_SPAN;
                        let j1 = (d + (WINDOW as i32 - 1)) >> LOG_SPAN;
                        let start = cnt[(j0 + CPAD as i32) as usize & CMASK];
                        let len = cnt[(j1 + 1 + CPAD as i32) as usize & CMASK].wrapping_sub(start);

                        // Word-1 prefix test: a single wrapping compare of the
                        // whole slot against the pre-shifted window start.
                        let tf16 = c2s.wrapping_add(n2s[ua]) & 0xFFFF_0000;

                        // Speculative branch-free look at the first three slots;
                        // a hit there, or a longer range whose tail holds a hit,
                        // goes to the exact scan.
                        let s0 = start as usize;
                        let mut hit = (slots[s0].wrapping_sub(tf16) < NFV16)
                            | (slots[s0 + 1].wrapping_sub(tf16) < NFV16)
                            | (slots[s0 + 2].wrapping_sub(tf16) < NFV16);
                        if len > 3 {
                            for k in 3..len {
                                let e = slots[start.wrapping_add(k) as usize];
                                hit |= e.wrapping_sub(tf16) < NFV16;
                            }
                        }
                        if hit {
                            let uh = ((ua << 8) | ub) as u32;
                            if let Some(seed) = scan_exact(&exact, slots, start, len, tf16, uh) {
                                return Some(seed);
                            }
                        }
                    }
                    while i > 0 && cub + fhv[(i - 1) & (RUN_LEN - 1)] as i32 >= next_p - qbase {
                        i -= 1;
                    }
                    fcur[ub] = i as u16;
                }
            }
        }
        None
    }

    /// Convenience wrapper using the default bucket span.
    #[inline]
    pub fn recover_default(
        &mut self,
        sector: &[u8; SECTOR_LEN],
        stream_sector: u32,
    ) -> Option<u32> {
        self.recover::<DEFAULT_LOG_SPAN>(sector, stream_sector)
    }
}

/// Bucket span chosen by benchmarking; see the README.
pub const DEFAULT_LOG_SPAN: u32 = 7;

pub mod flat;
pub mod params;

/// Component glue for the `datboi:transform@1` world (D89 epoch); wasm32-only
/// so the analyzer's native build of the solver carries no guest bindings.
#[cfg(target_arch = "wasm32")]
mod component;

#[cfg(test)]
mod tests;
