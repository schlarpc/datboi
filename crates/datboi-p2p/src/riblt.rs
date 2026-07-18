//! Rateless IBLT set-reconciliation codec (D100).
//!
//! A port of the reference Go implementation of "Practical Rateless Set
//! Reconciliation" (Yang, Gilad, Alizadeh — SIGCOMM 2024;
//! `github.com/yangl1996/riblt`, MIT), const-generic over the symbol
//! width `N` (D100 amendment: the algorithm is width-agnostic — the one
//! shape it resists is per-ELEMENT variable width, since peeling is XOR
//! algebra over a fixed-length domain — so width is a per-scope protocol
//! constant, one width per stream). The v1 wire instantiation is `N = 32`
//! ([`Symbol`], bare blake3); a future scope in another hash algebra
//! (sha1-shaped, alias-pair-shaped) instantiates its own width and MUST
//! bring its own goldens before becoming protocol surface.
//!
//! Ported statement-for-statement where the borrow checker allows,
//! because the correctness proof is DIFFERENTIAL, not a re-derivation:
//! the vendored reference (testdata/riblt/gen) generates committed golden
//! vectors, and the tests below pin our encoder byte-for-byte and our
//! decoder's exact diff recovery + symbol count against them — at
//! `N = 32`, the only reference-pinned width. Deviate from the reference
//! and the goldens catch it. The property tests then cover what goldens
//! can't: arbitrary set shapes, insertion-order invariance, duplicate
//! inputs, adversarial coded streams, and the width-genericity itself
//! (the invariants re-proven at non-32 widths).
//!
//! Shape: for any set, an infinite deterministic sequence of *coded
//! symbols* exists (each an XOR-sum of a pseudo-random subset of the set,
//! with a summed keyed checksum and a signed degree count). Prefixes of
//! two sets' sequences suffice to compute their symmetric difference, and
//! the prefix length needed is ~1.35× the difference size — independent
//! of the set sizes. The responder streams its sequence; the initiator
//! [`Decoder`] peels against its own set (the prior) until
//! [`Decoder::decoded`], then tells the responder to stop (docs/p2p.md
//! § Reconciliation).
//!
//! Correct-by-construction posture:
//! - Both codec halves are built FROM a complete set ([`Encoder::new`],
//!   [`Decoder::new`]) — the reference's "adding a symbol after coding
//!   starts is undefined behavior" contract is unexpressible here, not
//!   merely documented.
//! - Inputs dedupe at the boundary: a duplicated element would XOR-cancel
//!   inside the sketch and corrupt it silently, so the constructors take
//!   multisets and reconcile the underlying set.
//! - A coded stream that violates the peeling invariant (possible only
//!   for a malformed/adversarial encoder, argued below) flips the decoder
//!   into a terminal [`Decoder::is_malformed`] state instead of panicking
//!   or stalling silently; the transport layer refuses the peer.
//!
//! Cost model (the reference's, preserved): a source symbol participates
//! in coded index `i` with probability `1/(1+i/2)`, so encoding `m`
//! symbols over a set of `n` costs `O(n·log m)` applications amortized
//! (each `O(log n)` through the mapping heap), and decoding a difference
//! of `d` peels `O(d·log m)` applications — nothing is quadratic in set
//! size, coded-stream length, or difference size.
//!
//! The checksum is SipHash-2-4 under fixed protocol keys — keyed so an
//! adversary cannot craft symbols whose checksums cancel (the paper's
//! robustness argument rides the hash being unpredictable), fixed because
//! both sides must sum identical checksums for peeling to cancel them.

use std::collections::HashSet;
use std::hash::Hasher as _;

/// The v1 source symbol: a bare blake3 hash — the width both live wire
/// scopes use (D100/D102). Other widths instantiate the generic types
/// directly.
pub type Symbol = [u8; 32];

/// Wire size of one v1 (32-byte-symbol) coded symbol:
/// 32 XOR-sum ‖ 8 checksum LE ‖ 8 count LE. The generic form is
/// [`CodedSymbol::WIRE_LEN`].
pub const CODED_SYMBOL_LEN: usize = CodedSymbol::<32>::WIRE_LEN;

// Protocol constants (D100): the keys spell the surface they serve. Any
// change is a wire break — the goldens pin them. Shared across widths on
// purpose: two scopes never mix streams, so cross-width domain
// separation buys nothing, and per-width keys would be one more thing a
// new scope could get wrong.
const SIPHASH_K0: u64 = u64::from_le_bytes(*b"datboi/r");
const SIPHASH_K1: u64 = u64::from_le_bytes(*b"iblt/1\0\0");

const ADD: i64 = 1;
const REMOVE: i64 = -1;

/// SipHash-2-4 of a source symbol under the protocol keys — the checksum
/// summed into coded symbols, and the seed of the symbol's index mapping.
#[inline]
#[must_use]
pub fn hash_symbol<const N: usize>(symbol: &[u8; N]) -> u64 {
    let mut h = siphasher::sip::SipHasher24::new_with_keys(SIPHASH_K0, SIPHASH_K1);
    h.write(symbol);
    h.finish()
}

/// XOR one symbol into another: u64 lanes for the width's 8-byte
/// prefix, a byte loop for the ragged tail (the innermost operation of
/// the whole codec; whole-width byte loops vectorize less reliably).
/// Endianness is irrelevant under pure XOR.
#[inline]
fn xor_in_place<const N: usize>(a: &mut [u8; N], b: &[u8; N]) {
    let lanes = N / 8;
    for i in 0..lanes {
        let lane = u64::from_ne_bytes(a[i * 8..][..8].try_into().expect("8-byte lane"))
            ^ u64::from_ne_bytes(b[i * 8..][..8].try_into().expect("8-byte lane"));
        a[i * 8..][..8].copy_from_slice(&lane.to_ne_bytes());
    }
    for i in lanes * 8..N {
        a[i] ^= b[i];
    }
}

/// A source symbol bundled with its checksum (reference: `HashedSymbol`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HashedSymbol<const N: usize> {
    symbol: [u8; N],
    hash: u64,
}

impl<const N: usize> HashedSymbol<N> {
    #[inline]
    fn new(symbol: [u8; N]) -> Self {
        let hash = hash_symbol(&symbol);
        Self { symbol, hash }
    }
}

/// One coded symbol (reference: `CodedSymbol`): the XOR-sum of the source
/// symbols mapped to this index, the XOR-sum of their checksums, and the
/// signed count of how many were added minus removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodedSymbol<const N: usize> {
    pub symbol: [u8; N],
    pub hash: u64,
    pub count: i64,
}

/// Written out (not derived) because `[u8; N]: Default` is not yet
/// implemented for arbitrary `N` in std.
impl<const N: usize> Default for CodedSymbol<N> {
    fn default() -> Self {
        Self {
            symbol: [0; N],
            hash: 0,
            count: 0,
        }
    }
}

impl<const N: usize> CodedSymbol<N> {
    /// Wire size of one coded symbol at this width:
    /// N XOR-sum ‖ 8 checksum LE ‖ 8 count LE.
    pub const WIRE_LEN: usize = N + 16;

    #[inline]
    fn apply(&mut self, s: &HashedSymbol<N>, direction: i64) {
        xor_in_place(&mut self.symbol, &s.symbol);
        self.hash ^= s.hash;
        self.count += direction;
    }

    /// Fixed wire encoding (D100): symbol ‖ hash LE ‖ count LE, exactly
    /// [`Self::WIRE_LEN`] bytes into `out`. A bijection over all
    /// WIRE_LEN-byte strings — every wire record parses, so untrusted
    /// input is handled by decode semantics, not framing. Slice-shaped
    /// because stable Rust cannot spell `[u8; N + 16]` in a signature.
    ///
    /// # Panics
    /// If `out.len() != Self::WIRE_LEN` — a caller bug, not a data
    /// condition.
    pub fn write_to(&self, out: &mut [u8]) {
        assert_eq!(out.len(), Self::WIRE_LEN, "coded-symbol buffer size");
        out[..N].copy_from_slice(&self.symbol);
        out[N..N + 8].copy_from_slice(&self.hash.to_le_bytes());
        out[N + 8..].copy_from_slice(&self.count.to_le_bytes());
    }

    /// Convenience twin of [`Self::write_to`] (tests, cold paths — it
    /// allocates).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![0u8; Self::WIRE_LEN];
        self.write_to(&mut out);
        out
    }

    /// # Panics
    /// If `bytes.len() != Self::WIRE_LEN` — a caller bug, not a data
    /// condition (the transport reads exact-size records).
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        assert_eq!(bytes.len(), Self::WIRE_LEN, "coded-symbol record size");
        let mut symbol = [0u8; N];
        symbol.copy_from_slice(&bytes[..N]);
        let hash = u64::from_le_bytes(bytes[N..N + 8].try_into().expect("8 bytes"));
        let count = i64::from_le_bytes(bytes[N + 8..].try_into().expect("8 bytes"));
        Self {
            symbol,
            hash,
            count,
        }
    }
}

/// The index-mapping generator (reference: `randomMapping`): seeded with a
/// symbol's checksum, it emits the strictly increasing coded-symbol
/// indices the symbol participates in; index `i` appears with probability
/// `1/(1+i/2)`. The float update is the reference's exact expression —
/// IEEE-754 f64 ops (mul, div, sqrt, ceil) are correctly rounded in both
/// languages, so the sequences agree bit-for-bit (the goldens prove it).
/// The one deviation is `saturating_add`: identical in every reachable
/// state (indices stay far below 2^63 under any real stream length — the
/// index grows by at most ~2^32× per step and only advances while below
/// the coded-stream length), it only pins the astronomically unreachable
/// tail to MAX instead of wrapping, because a wrapped index would break
/// the monotonicity the mapping heap's ordering rests on.
#[derive(Clone, Copy, Debug)]
struct RandomMapping {
    prng: u64,
    last_index: u64,
}

impl RandomMapping {
    #[inline]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn next_index(&mut self) -> u64 {
        self.prng = self.prng.wrapping_mul(0xda94_2042_e4dd_58b5);
        let r = self.prng;
        let step = (((self.last_index as f64) + 1.5)
            * (((1u64 << 32) as f64) / ((r as f64) + 1.0).sqrt() - 1.0))
            .ceil() as u64;
        self.last_index = self.last_index.saturating_add(step);
        self.last_index
    }
}

/// A source symbol's next pending coded index (reference: `symbolMapping`).
#[derive(Clone, Copy, Debug)]
struct SymbolMapping {
    source_idx: usize,
    coded_idx: u64,
}

// Min-heap on coded_idx (reference: mappingHeap, a partial copy of Go's
// container/heap). Hand-rolled rather than BinaryHeap because the
// reference mutates the head in place and re-sifts — and the port must
// visit symbols in the identical order for byte-identical output.
#[inline]
fn fix_head(q: &mut [SymbolMapping]) {
    let mut curr = 0usize;
    loop {
        let mut child = curr * 2 + 1;
        if child >= q.len() {
            break;
        }
        let rc = child + 1;
        if rc < q.len() && q[rc].coded_idx < q[child].coded_idx {
            child = rc;
        }
        if q[curr].coded_idx <= q[child].coded_idx {
            break;
        }
        q.swap(curr, child);
        curr = child;
    }
}

#[inline]
fn fix_tail(q: &mut [SymbolMapping]) {
    let mut curr = q.len() - 1;
    while curr > 0 {
        let parent = (curr - 1) / 2;
        if q[parent].coded_idx <= q[curr].coded_idx {
            break;
        }
        q.swap(parent, curr);
        curr = parent;
    }
}

/// A set of source symbols plus their mapping state (reference:
/// `codingWindow`) — the machinery shared by the encoder and the
/// decoder's three internal windows.
struct CodingWindow<const N: usize> {
    symbols: Vec<HashedSymbol<N>>,
    mappings: Vec<RandomMapping>,
    queue: Vec<SymbolMapping>,
    next_idx: u64,
}

impl<const N: usize> Default for CodingWindow<N> {
    fn default() -> Self {
        Self {
            symbols: Vec::new(),
            mappings: Vec::new(),
            queue: Vec::new(),
            next_idx: 0,
        }
    }
}

impl<const N: usize> CodingWindow<N> {
    fn add_hashed_symbol(&mut self, t: HashedSymbol<N>) {
        self.add_hashed_symbol_with_mapping(
            t,
            RandomMapping {
                prng: t.hash,
                last_index: 0,
            },
        );
    }

    fn add_hashed_symbol_with_mapping(&mut self, t: HashedSymbol<N>, m: RandomMapping) {
        self.symbols.push(t);
        self.mappings.push(m);
        self.queue.push(SymbolMapping {
            source_idx: self.symbols.len() - 1,
            coded_idx: m.last_index,
        });
        fix_tail(&mut self.queue);
    }

    /// Apply every source symbol mapped to the next coded index onto `cw`,
    /// advance their mappings, and advance the window's cursor.
    fn apply_window(&mut self, mut cw: CodedSymbol<N>, direction: i64) -> CodedSymbol<N> {
        if self.queue.is_empty() {
            self.next_idx += 1;
            return cw;
        }
        while self.queue[0].coded_idx == self.next_idx {
            let src = self.queue[0].source_idx;
            cw.apply(&self.symbols[src], direction);
            let next = self.mappings[src].next_index();
            self.queue[0].coded_idx = next;
            fix_head(&mut self.queue);
        }
        self.next_idx += 1;
        cw
    }
}

/// Deduplicate a multiset of symbols into hashed form — the constructor
/// boundary both codec halves share. A duplicated element would
/// XOR-cancel inside the sketch (present-twice reads as absent, with a
/// count residue), so set semantics are enforced here rather than
/// assumed of the caller.
fn dedup_symbols<const N: usize>(
    symbols: impl IntoIterator<Item = [u8; N]>,
) -> Vec<HashedSymbol<N>> {
    let mut seen: HashSet<[u8; N]> = HashSet::new();
    symbols
        .into_iter()
        .filter(|s| seen.insert(*s))
        .map(HashedSymbol::new)
        .collect()
}

/// A re-iterable, stable view of a symbol set — the streaming encoder's
/// source (D100 amendment). Two contract halves the encoder CANNOT
/// enforce, so every implementation must:
///
/// 1. **Stability**: every `for_each` call visits the same set. The
///    sqlite implementation gets this from a read transaction held
///    across all passes (WAL snapshot isolation); a slice is stable
///    trivially.
/// 2. **Distinctness**: no element visits twice within a pass. A
///    duplicate XOR-cancels inside the sketch and corrupts it silently
///    — the in-memory [`Encoder`] dedups at its boundary, but a
///    streaming pass cannot without the O(n) state this trait exists to
///    remove, so the source owes it (a structurally-DISTINCT query, a
///    deduped slice).
pub trait SetSnapshot<const N: usize> {
    type Error;
    /// Visit every element of the set exactly once, in any order.
    fn for_each(&mut self, f: &mut dyn FnMut([u8; N])) -> Result<(), Self::Error>;
}

/// Slices are snapshots of themselves. Callers owe distinctness (the
/// trait contract) — test and decoder-prior shapes, where sets are
/// deduped upstream.
impl<const N: usize> SetSnapshot<N> for &[[u8; N]] {
    type Error = std::convert::Infallible;

    fn for_each(&mut self, f: &mut dyn FnMut([u8; N])) -> Result<(), Self::Error> {
        for s in self.iter() {
            f(*s);
        }
        Ok(())
    }
}

/// Encode coded symbols `[start, start + out.len())` of the set's
/// sequence in ONE pass over `set`, replaying each symbol's index
/// mapping from zero (D100 amendment: memory is O(out.len()), never
/// O(set)). Emits byte-identical symbols to [`Encoder`] — the
/// differential property below pins it — so a responder can stream an
/// unbounded sequence in exponentially growing blocks at one set-scan
/// per block, holding only the current block.
pub fn encode_block<const N: usize, S: SetSnapshot<N>>(
    set: &mut S,
    start: u64,
    out: &mut [CodedSymbol<N>],
) -> Result<(), S::Error> {
    out.fill(CodedSymbol::default());
    let end = start + out.len() as u64;
    set.for_each(&mut |symbol| {
        let hs = HashedSymbol::new(symbol);
        let mut m = RandomMapping {
            prng: hs.hash,
            last_index: 0,
        };
        // A mapping's first index is 0 (before any advance), matching
        // the incremental window's initial queue entry.
        loop {
            let idx = m.last_index;
            if idx >= end {
                break;
            }
            if idx >= start {
                #[allow(clippy::cast_possible_truncation)]
                out[(idx - start) as usize].apply(&hs, ADD);
            }
            m.next_index();
        }
    })
}

/// Incremental encoder over a fixed set: constructed from the complete
/// set, then produces the set's coded-symbol sequence one at a time,
/// forever. There is no way to add a symbol after coding starts — the
/// reference's undefined-behavior contract, made unexpressible. Memory
/// is O(set) — the daemon's recon responder streams via
/// [`encode_block`] instead (D100 amendment); this shape remains for
/// the decoder's windows, priors that are resident anyway, and the
/// differential tests.
///
/// The emitted sequence is a pure function of the SET: insertion order
/// and duplicates cannot affect it (each symbol's index mapping depends
/// only on its own checksum, and coded sums are commutative — the
/// property tests pin this).
pub struct Encoder<const N: usize = 32> {
    window: CodingWindow<N>,
}

impl<const N: usize> Encoder<N> {
    #[must_use]
    pub fn new(set: impl IntoIterator<Item = [u8; N]>) -> Self {
        let mut window = CodingWindow::default();
        for symbol in dedup_symbols(set) {
            window.add_hashed_symbol(symbol);
        }
        Self { window }
    }

    /// Number of distinct symbols in the encoded set.
    #[must_use]
    pub fn set_len(&self) -> usize {
        self.window.symbols.len()
    }

    pub fn produce_next_coded_symbol(&mut self) -> CodedSymbol<N> {
        self.window.apply_window(CodedSymbol::default(), ADD)
    }
}

/// Streaming decoder: constructed with the local set B (the prior),
/// consumes the remote set A's coded symbols in order, and peels until
/// the symmetric difference is recovered — [`Decoder::remote`] is A∖B
/// (what to fetch), [`Decoder::local`] is B∖A (never leaves this
/// process; the D100 asymmetric reveal).
pub struct Decoder<const N: usize = 32> {
    /// Coded symbols received so far, mutated as decoded symbols peel off.
    cs: Vec<CodedSymbol<N>>,
    /// Recovered symbols exclusive to the decoder (B∖A).
    local: CodingWindow<N>,
    /// The decoder's own set (the prior).
    window: CodingWindow<N>,
    /// Recovered symbols exclusive to the encoder (A∖B).
    remote: CodingWindow<N>,
    /// Indices of coded symbols currently decodable (degree ±1 with a
    /// matching checksum, or degree 0 with a zero checksum-sum).
    decodable: Vec<usize>,
    decoded: usize,
    /// Terminal: the stream violated the peeling invariant (a decodable
    /// entry left degree {-1, 0, 1}) — impossible for an honest encoder
    /// (peeling only ever removes a true member from the symbols it was
    /// actually summed into), so the stream is malformed or adversarial.
    malformed: bool,
}

impl<const N: usize> Decoder<N> {
    #[must_use]
    pub fn new(prior: impl IntoIterator<Item = [u8; N]>) -> Self {
        let mut window = CodingWindow::default();
        for symbol in dedup_symbols(prior) {
            window.add_hashed_symbol(symbol);
        }
        Self {
            cs: Vec::new(),
            local: CodingWindow::default(),
            window,
            remote: CodingWindow::default(),
            decodable: Vec::new(),
            decoded: 0,
            malformed: false,
        }
    }

    /// Number of distinct symbols in the prior.
    #[must_use]
    pub fn prior_len(&self) -> usize {
        self.window.symbols.len()
    }

    /// True iff every coded symbol received so far has been decoded —
    /// at which point `remote`/`local` hold the full symmetric difference.
    /// Never true for a malformed stream.
    #[must_use]
    pub fn decoded(&self) -> bool {
        !self.malformed && self.decoded == self.cs.len()
    }

    /// True iff the stream violated the codec's invariants (see the
    /// field docs). Terminal: the transport should drop the peer; no
    /// further input can repair the sketch.
    #[must_use]
    pub fn is_malformed(&self) -> bool {
        self.malformed
    }

    /// Symbols present at the encoder but not here (A∖B).
    pub fn remote(&self) -> impl Iterator<Item = &[u8; N]> {
        self.remote.symbols.iter().map(|s| &s.symbol)
    }

    /// Symbols present here but not at the encoder (B∖A).
    pub fn local(&self) -> impl Iterator<Item = &[u8; N]> {
        self.local.symbols.iter().map(|s| &s.symbol)
    }

    /// Consume the next coded symbol of the remote sequence, in order.
    /// A no-op once the stream is malformed.
    pub fn add_coded_symbol(&mut self, c: CodedSymbol<N>) {
        if self.malformed {
            return;
        }
        let c = self.window.apply_window(c, REMOVE);
        let c = self.remote.apply_window(c, REMOVE);
        let c = self.local.apply_window(c, ADD);
        self.cs.push(c);
        if ((c.count == 1 || c.count == -1) && c.hash == hash_symbol(&c.symbol))
            || (c.count == 0 && c.hash == 0)
        {
            self.decodable.push(self.cs.len() - 1);
        }
    }

    /// Map a freshly recovered symbol onto every received coded symbol it
    /// participates in, collecting any that become decodable. Returns the
    /// mapping state so the symbol's window can extend it to future
    /// arrivals.
    fn apply_new_symbol(&mut self, t: &HashedSymbol<N>, direction: i64) -> RandomMapping {
        let mut m = RandomMapping {
            prng: t.hash,
            last_index: 0,
        };
        #[allow(clippy::cast_possible_truncation)]
        while (m.last_index as usize) < self.cs.len() {
            let cidx = m.last_index as usize;
            self.cs[cidx].apply(t, direction);
            // Only degree ±1 enters the decodable list here: a decodable
            // symbol never becomes undecodable (peeling only removes
            // members), and a symbol reaching degree 0 was necessarily
            // decodable at ±1 earlier — pushing it again would visit it
            // twice (the reference's invariant, argued in its comments).
            if (self.cs[cidx].count == -1 || self.cs[cidx].count == 1)
                && self.cs[cidx].hash == hash_symbol(&self.cs[cidx].symbol)
            {
                self.decodable.push(cidx);
            }
            m.next_index();
        }
        m
    }

    /// Peel every decodable coded symbol, cascading (peeling one symbol
    /// can make others decodable; the queue grows while it is walked).
    pub fn try_decode(&mut self) {
        if self.malformed {
            return;
        }
        let mut didx = 0;
        while didx < self.decodable.len() {
            let cidx = self.decodable[didx];
            let c = self.cs[cidx];
            match c.count {
                1 => {
                    let ns = HashedSymbol {
                        symbol: c.symbol,
                        hash: c.hash,
                    };
                    let m = self.apply_new_symbol(&ns, REMOVE);
                    self.remote.add_hashed_symbol_with_mapping(ns, m);
                    self.decoded += 1;
                }
                -1 => {
                    let ns = HashedSymbol {
                        symbol: c.symbol,
                        hash: c.hash,
                    };
                    let m = self.apply_new_symbol(&ns, ADD);
                    self.local.add_hashed_symbol_with_mapping(ns, m);
                    self.decoded += 1;
                }
                0 => self.decoded += 1,
                // The reference panics here; its invariant argument only
                // covers honest encoders, and coded symbols come off the
                // wire. Refuse the stream instead of aborting the daemon.
                _ => {
                    self.malformed = true;
                    self.decodable.clear();
                    return;
                }
            }
            didx += 1;
        }
        self.decodable.clear();
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Deterministic 32-byte test symbols shared with the Go golden
    /// generator (testdata/riblt/gen): four splitmix64 outputs, LE.
    fn test_symbol(i: u64) -> Symbol {
        fn splitmix64(x: u64) -> u64 {
            let mut z = x.wrapping_add(0x9E37_79B9_7F4B_9C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        let mut out = [0u8; 32];
        for j in 0..4u64 {
            // Wrapping, like the generator's Go uint64 arithmetic.
            out[(j as usize) * 8..(j as usize + 1) * 8]
                .copy_from_slice(&splitmix64(i.wrapping_mul(4).wrapping_add(j)).to_le_bytes());
        }
        out
    }

    /// Drive one full reconciliation in memory; returns (symbols used,
    /// remote diff, local diff), the loop the reference's example runs.
    /// Panics past `cap` symbols — convergence failure is a test failure.
    fn reconcile_capped(
        alice: &[Symbol],
        bob: &[Symbol],
        cap: usize,
    ) -> (usize, Vec<Symbol>, Vec<Symbol>) {
        let mut enc = Encoder::new(alice.iter().copied());
        let mut dec = Decoder::new(bob.iter().copied());
        let mut used = 0;
        loop {
            dec.add_coded_symbol(enc.produce_next_coded_symbol());
            used += 1;
            dec.try_decode();
            assert!(!dec.is_malformed(), "honest stream flagged malformed");
            if dec.decoded() {
                break;
            }
            assert!(used < cap, "reconciliation did not converge in {cap}");
        }
        let mut remote: Vec<Symbol> = dec.remote().copied().collect();
        let mut local: Vec<Symbol> = dec.local().copied().collect();
        remote.sort_unstable();
        local.sort_unstable();
        (used, remote, local)
    }

    fn reconcile(alice: &[Symbol], bob: &[Symbol]) -> (usize, Vec<Symbol>, Vec<Symbol>) {
        reconcile_capped(alice, bob, 100_000)
    }

    #[test]
    fn identical_sets_decode_immediately() {
        let set: Vec<Symbol> = (0..500).map(test_symbol).collect();
        let (used, remote, local) = reconcile(&set, &set);
        assert_eq!(used, 1, "an all-cancelled first symbol suffices");
        assert!(remote.is_empty());
        assert!(local.is_empty());
    }

    #[test]
    fn overlapping_windows_recover_the_exact_diff() {
        // Alice 0..1000, Bob 20..1020: 20 exclusive each side.
        let alice: Vec<Symbol> = (0..1000).map(test_symbol).collect();
        let bob: Vec<Symbol> = (20..1020).map(test_symbol).collect();
        let (used, remote, local) = reconcile(&alice, &bob);
        let mut want_remote: Vec<Symbol> = (0..20).map(test_symbol).collect();
        let mut want_local: Vec<Symbol> = (1000..1020).map(test_symbol).collect();
        want_remote.sort_unstable();
        want_local.sort_unstable();
        assert_eq!(remote, want_remote, "A∖B recovered exactly");
        assert_eq!(local, want_local, "B∖A recovered exactly");
        // ~1.35×d is the paper's asymptotic constant; small diffs run a
        // little hotter. Sanity-bound the overhead, don't pin it.
        assert!(used >= 40, "cannot beat the information bound of d=40");
        assert!(used < 40 * 4, "overhead blew past sanity: {used} symbols");
    }

    #[test]
    fn empty_prior_recovers_the_whole_remote_set() {
        let alice: Vec<Symbol> = (0..100).map(test_symbol).collect();
        let (used, remote, local) = reconcile(&alice, &[]);
        assert_eq!(remote.len(), 100);
        assert!(local.is_empty());
        assert!(used >= 100);
    }

    #[test]
    fn empty_remote_recovers_the_whole_local_set() {
        let bob: Vec<Symbol> = (0..50).map(test_symbol).collect();
        let (_used, remote, local) = reconcile(&[], &bob);
        assert!(remote.is_empty());
        assert_eq!(local.len(), 50);
    }

    /// The block encoder reproduces the golden streams — pinning the
    /// streaming path to the reference through the same artifact chain
    /// (block == incremental == Go), here directly against the bytes.
    #[test]
    fn golden_encoder_streams_match_block_encoding() {
        for (n, golden) in [
            (1u64, &include_bytes!("../testdata/riblt/golden/encoder_1.bin")[..]),
            (100, &include_bytes!("../testdata/riblt/golden/encoder_100.bin")[..]),
        ] {
            let set: Vec<Symbol> = (0..n).map(test_symbol).collect();
            // One 256-symbol block, and ragged blocks (1, 3, 60, rest).
            for cuts in [vec![256usize], vec![1, 3, 60, 192]] {
                let mut stream = Vec::with_capacity(golden.len());
                let mut start = 0u64;
                for len in cuts {
                    let mut block = vec![CodedSymbol::default(); len];
                    encode_block(&mut set.as_slice(), start, &mut block)
                        .expect("infallible source");
                    for c in &block {
                        stream.extend_from_slice(&c.to_bytes());
                    }
                    start += len as u64;
                }
                assert_eq!(stream, golden, "block encoding diverged over {n} symbols");
            }
        }
    }

    #[test]
    fn coded_symbol_wire_roundtrip() {
        let mut enc = Encoder::new((0..10).map(test_symbol));
        for _ in 0..100 {
            let c = enc.produce_next_coded_symbol();
            assert_eq!(CodedSymbol::from_bytes(&c.to_bytes()), c);
        }
    }

    // ---- Differential goldens (D100) ----
    // Generated by testdata/riblt/gen (the vendored-by-pin REFERENCE Go
    // implementation driving datboi's symbol type). These pin the port to
    // the paper's artifact: encoder output byte-for-byte, decoder diff
    // recovery and exact symbol count, and the SipHash checksum itself.

    const GOLDEN_CASES: &str = include_str!("../testdata/riblt/golden/cases.txt");

    fn hex32(s: &str) -> Symbol {
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("hex");
        }
        out
    }

    fn to_hex(s: &Symbol) -> String {
        s.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn golden_hashes_match_the_reference() {
        let mut seen = 0;
        for line in GOLDEN_CASES.lines().filter(|l| l.starts_with("hash ")) {
            let mut parts = line.split_whitespace().skip(1);
            let index: u64 = parts.next().expect("index").parse().expect("u64");
            let symbol = hex32(parts.next().expect("symbol"));
            let hash: u64 = parts.next().expect("hash").parse().expect("u64");
            assert_eq!(test_symbol(index), symbol, "symbol constructor diverged at {index}");
            assert_eq!(hash_symbol(&symbol), hash, "SipHash checksum diverged at {index}");
            seen += 1;
        }
        assert_eq!(seen, 5, "golden file shrank");
    }

    #[test]
    fn golden_encoder_streams_match_byte_for_byte() {
        for (n, golden) in [
            (1u64, &include_bytes!("../testdata/riblt/golden/encoder_1.bin")[..]),
            (3, &include_bytes!("../testdata/riblt/golden/encoder_3.bin")[..]),
            (100, &include_bytes!("../testdata/riblt/golden/encoder_100.bin")[..]),
        ] {
            let mut enc = Encoder::new((0..n).map(test_symbol));
            let mut stream = Vec::with_capacity(golden.len());
            for _ in 0..256 {
                stream.extend_from_slice(&enc.produce_next_coded_symbol().to_bytes());
            }
            assert_eq!(
                stream, golden,
                "encoder stream over {n} symbols diverged from the reference"
            );
        }
    }

    #[test]
    fn golden_decode_cases_match_exactly() {
        let mut lines = GOLDEN_CASES.lines().peekable();
        let mut cases = 0;
        while let Some(line) = lines.next() {
            let Some(rest) = line.strip_prefix("case ") else {
                continue;
            };
            let mut parts = rest.split_whitespace();
            let name = parts.next().expect("name");
            let range = |p: &mut dyn Iterator<Item = &str>| -> u64 {
                p.next().expect("bound").parse().expect("u64")
            };
            let (a0, a1, b0, b1) = (
                range(&mut parts),
                range(&mut parts),
                range(&mut parts),
                range(&mut parts),
            );
            let want_used: usize = parts.next().expect("used").parse().expect("usize");
            let mut want_remote = Vec::new();
            let mut want_local = Vec::new();
            for entry in lines.by_ref() {
                if entry == "end" {
                    break;
                } else if let Some(h) = entry.strip_prefix("remote ") {
                    want_remote.push(h.to_owned());
                } else if let Some(h) = entry.strip_prefix("local ") {
                    want_local.push(h.to_owned());
                } else {
                    panic!("unexpected golden line: {entry}");
                }
            }

            let alice: Vec<Symbol> = (a0..a1).map(test_symbol).collect();
            let bob: Vec<Symbol> = (b0..b1).map(test_symbol).collect();
            let (used, remote, local) = reconcile(&alice, &bob);
            assert_eq!(used, want_used, "case {name}: symbol count diverged");
            let remote: Vec<String> = remote.iter().map(to_hex).collect();
            let local: Vec<String> = local.iter().map(to_hex).collect();
            assert_eq!(remote, want_remote, "case {name}: remote diff diverged");
            assert_eq!(local, want_local, "case {name}: local diff diverged");
            cases += 1;
        }
        assert_eq!(cases, 5, "golden file shrank");
    }

    // ---- Properties ----
    // The goldens pin exact reference behavior on fixed cases; these pin
    // the invariants on arbitrary ones.

    /// Three disjoint index pools (tags keep them disjoint by
    /// construction) → (shared, a_only, b_only) symbol groups.
    fn set_shapes() -> impl Strategy<Value = (Vec<Symbol>, Vec<Symbol>, Vec<Symbol>)> {
        (0usize..120, 0usize..40, 0usize..40, any::<u64>()).prop_map(
            |(shared, a_only, b_only, seed)| {
                let sym = move |tag: u64, i: usize| {
                    let mut s = test_symbol(seed.wrapping_add(tag.wrapping_mul(1 << 40)));
                    let t = test_symbol((i as u64) | (tag << 48));
                    xor_in_place(&mut s, &t);
                    s
                };
                (
                    (0..shared).map(|i| sym(1, i)).collect(),
                    (0..a_only).map(|i| sym(2, i)).collect(),
                    (0..b_only).map(|i| sym(3, i)).collect(),
                )
            },
        )
    }

    proptest! {
        /// The load-bearing property: for arbitrary set shapes the
        /// decoder recovers EXACTLY the symmetric difference, both
        /// directions, from an honest stream.
        #[test]
        fn recovers_the_exact_symmetric_difference(
            (shared, a_only, b_only) in set_shapes()
        ) {
            let alice: Vec<Symbol> =
                shared.iter().chain(&a_only).copied().collect();
            let bob: Vec<Symbol> =
                shared.iter().chain(&b_only).copied().collect();
            let (_used, remote, local) = reconcile_capped(&alice, &bob, 50_000);
            let mut want_remote = a_only.clone();
            let mut want_local = b_only.clone();
            want_remote.sort_unstable();
            want_local.sort_unstable();
            prop_assert_eq!(remote, want_remote);
            prop_assert_eq!(local, want_local);
        }

        /// The coded stream is a pure function of the SET: insertion
        /// order and duplicated elements cannot change a single byte.
        /// (Load-bearing for the protocol: the two peers enumerate their
        /// sets in unrelated orders.)
        #[test]
        fn stream_is_a_pure_function_of_the_set(
            n in 1usize..80,
            seed in any::<u64>(),
            dup in any::<prop::sample::Index>(),
        ) {
            let set: Vec<Symbol> =
                (0..n as u64).map(|i| test_symbol(i.wrapping_add(seed))).collect();
            let mut shuffled = set.clone();
            shuffled.rotate_left(n / 3);
            shuffled.reverse();
            // ...and one element repeated: multiset in, set out.
            shuffled.push(set[dup.index(n)]);

            let mut a = Encoder::new(set.iter().copied());
            let mut b = Encoder::new(shuffled.iter().copied());
            prop_assert_eq!(a.set_len(), b.set_len());
            for _ in 0..96 {
                prop_assert_eq!(
                    a.produce_next_coded_symbol().to_bytes(),
                    b.produce_next_coded_symbol().to_bytes()
                );
            }
        }

        /// The streaming responder's load-bearing property (D100
        /// amendment): block encoding over ARBITRARY block cuts is
        /// byte-identical to the incremental encoder — so swapping the
        /// responder's memory shape cannot change a single wire byte.
        #[test]
        fn block_encoding_equals_incremental_for_any_cuts(
            n in 0usize..60,
            seed in any::<u64>(),
            cuts in prop::collection::vec(1usize..24, 1..8),
        ) {
            let set: Vec<Symbol> =
                (0..n as u64).map(|i| test_symbol(i.wrapping_add(seed))).collect();
            let mut inc = Encoder::new(set.iter().copied());
            let mut start = 0u64;
            for len in cuts {
                let mut block = vec![CodedSymbol::default(); len];
                encode_block(&mut set.as_slice(), start, &mut block)
                    .expect("infallible source");
                for c in &block {
                    prop_assert_eq!(
                        c.to_bytes(),
                        inc.produce_next_coded_symbol().to_bytes(),
                        "diverged at coded index {}", start
                    );
                }
                start += len as u64;
            }
        }

        /// The wire format is a bijection on 48-byte strings.
        #[test]
        fn wire_encoding_is_a_bijection(bytes in prop::collection::vec(any::<u8>(), 48)) {
            let record: [u8; 48] = bytes.as_slice().try_into().expect("48 bytes");
            let decoded = CodedSymbol::<32>::from_bytes(&record);
            prop_assert_eq!(decoded.to_bytes(), record);
        }

        /// Garbage coded streams never panic, never report a malformed
        /// stream as decoded, and the decoder survives to refuse further
        /// input. (The transport's byte-verified fetches make a LYING
        /// stream harmless; this pins that it also can't be a crash.)
        #[test]
        fn adversarial_streams_never_panic(
            prior_n in 0usize..30,
            records in prop::collection::vec(prop::collection::vec(any::<u8>(), 48), 1..60),
        ) {
            let mut dec = Decoder::new((0..prior_n as u64).map(test_symbol));
            for r in &records {
                let record: [u8; 48] = r.as_slice().try_into().expect("48 bytes");
                dec.add_coded_symbol(CodedSymbol::from_bytes(&record));
                dec.try_decode();
            }
            if dec.is_malformed() {
                prop_assert!(!dec.decoded(), "malformed stream claimed decoded");
                // Terminal: more input is a no-op, not a panic.
                dec.add_coded_symbol(CodedSymbol::default());
                dec.try_decode();
                prop_assert!(dec.is_malformed());
            }
        }
    }

    /// Width-genericity (D100 amendment): the codec's invariants hold at
    /// non-32 widths — exercised at the two shapes future scopes would
    /// instantiate (20 = sha1-shaped, 52 = sha1‖blake3 alias-pair). This
    /// is NOT a reference pin (goldens exist only for 32, the only wire
    /// width); a new width becoming protocol surface owes its own
    /// goldens first. Exact diff recovery, wire round-trip, and
    /// block == incremental are each re-proven per width.
    #[test]
    fn codec_invariants_hold_at_other_widths() {
        fn exercise<const N: usize>() {
            let sym = |i: u64| -> [u8; N] {
                let mut out = [0u8; N];
                for (j, b) in out.iter_mut().enumerate() {
                    *b = (test_symbol(i)[j % 32]).wrapping_add(j as u8);
                }
                out
            };
            let alice: Vec<[u8; N]> = (0..300).map(sym).collect();
            let bob: Vec<[u8; N]> = (30..330).map(sym).collect();

            let mut enc = Encoder::<N>::new(alice.iter().copied());
            let mut dec = Decoder::<N>::new(bob.iter().copied());
            let mut inc_stream: Vec<CodedSymbol<N>> = Vec::new();
            let mut used = 0usize;
            loop {
                let c = enc.produce_next_coded_symbol();
                // Wire round-trip at this width.
                assert_eq!(CodedSymbol::<N>::from_bytes(&c.to_bytes()), c);
                inc_stream.push(c);
                dec.add_coded_symbol(c);
                used += 1;
                dec.try_decode();
                assert!(!dec.is_malformed());
                if dec.decoded() {
                    break;
                }
                assert!(used < 10_000, "width {N} did not converge");
            }
            let mut remote: Vec<[u8; N]> = dec.remote().copied().collect();
            let mut local: Vec<[u8; N]> = dec.local().copied().collect();
            remote.sort_unstable();
            local.sort_unstable();
            let mut want_remote: Vec<[u8; N]> = (0..30).map(sym).collect();
            let mut want_local: Vec<[u8; N]> = (300..330).map(sym).collect();
            want_remote.sort_unstable();
            want_local.sort_unstable();
            assert_eq!(remote, want_remote, "A∖B at width {N}");
            assert_eq!(local, want_local, "B∖A at width {N}");

            // Block encoding stays byte-identical to incremental at this
            // width (ragged cuts).
            let mut start = 0u64;
            for len in [1usize, 7, 64, used.saturating_sub(72).max(1)] {
                let mut block = vec![CodedSymbol::<N>::default(); len];
                encode_block(&mut alice.as_slice(), start, &mut block).expect("infallible");
                for (i, c) in block.iter().enumerate() {
                    let at = start as usize + i;
                    if at < inc_stream.len() {
                        assert_eq!(c, &inc_stream[at], "width {N} diverged at {at}");
                    }
                }
                start += len as u64;
            }
        }
        exercise::<20>();
        exercise::<52>();
    }

    /// Scale guard (release-mode manual run: `cargo test -p datboi-p2p
    /// --release -- --ignored stress`): 100k×100k sets with a 400-element
    /// diff must reconcile in seconds and near the ~1.35×d constant —
    /// a quadratic regression fails this loudly or times out.
    #[test]
    #[ignore = "release-mode scale check"]
    fn stress_hundred_thousand_by_hundred_thousand() {
        let alice: Vec<Symbol> = (0..100_000).map(test_symbol).collect();
        let bob: Vec<Symbol> = (200..100_200).map(test_symbol).collect();
        let start = std::time::Instant::now();
        let (used, remote, local) = reconcile_capped(&alice, &bob, 10_000);
        let elapsed = start.elapsed();
        assert_eq!(remote.len(), 200);
        assert_eq!(local.len(), 200);
        assert!(used < 400 * 3, "overhead at scale: {used} for d=400");
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "reconciliation took {elapsed:?}"
        );
        eprintln!("stress: d=400 over 100k×100k in {used} symbols, {elapsed:?}");
    }
}
