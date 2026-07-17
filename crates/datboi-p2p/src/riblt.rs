//! Rateless IBLT set-reconciliation codec (D100).
//!
//! A `[u8; 32]`-specialized port of the reference Go implementation of
//! "Practical Rateless Set Reconciliation" (Yang, Gilad, Alizadeh —
//! SIGCOMM 2024; `github.com/yangl1996/riblt`, MIT). Ported
//! statement-for-statement where the borrow checker allows, because the
//! correctness proof is DIFFERENTIAL, not a re-derivation: the vendored
//! reference (testdata/riblt/gen) generates committed golden vectors, and
//! the tests below pin our encoder byte-for-byte and our decoder's exact
//! diff recovery + symbol count against them. Deviate from the reference
//! and the goldens catch it.
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
//! The checksum is SipHash-2-4 under fixed protocol keys — keyed so an
//! adversary cannot craft symbols whose checksums cancel (the paper's
//! robustness argument rides the hash being unpredictable), fixed because
//! both sides must sum identical checksums for peeling to cancel them.

use std::hash::Hasher as _;

/// A source symbol: a bare blake3 hash, the one element type every
/// reconciled scope uses (D100 — recipe object hashes today).
pub type Symbol = [u8; 32];

/// Wire size of one coded symbol: 32 XOR-sum ‖ 8 checksum LE ‖ 8 count LE.
pub const CODED_SYMBOL_LEN: usize = 48;

// Protocol constants (D100): the keys spell the surface they serve. Any
// change is a wire break — the goldens pin them.
const SIPHASH_K0: u64 = u64::from_le_bytes(*b"datboi/r");
const SIPHASH_K1: u64 = u64::from_le_bytes(*b"iblt/1\0\0");

const ADD: i64 = 1;
const REMOVE: i64 = -1;

/// SipHash-2-4 of a source symbol under the protocol keys — the checksum
/// summed into coded symbols, and the seed of the symbol's index mapping.
#[must_use]
pub fn hash_symbol(symbol: &Symbol) -> u64 {
    let mut h = siphasher::sip::SipHasher24::new_with_keys(SIPHASH_K0, SIPHASH_K1);
    h.write(symbol);
    h.finish()
}

/// A source symbol bundled with its checksum (reference: `HashedSymbol`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashedSymbol {
    pub symbol: Symbol,
    pub hash: u64,
}

impl HashedSymbol {
    #[must_use]
    pub fn new(symbol: Symbol) -> Self {
        let hash = hash_symbol(&symbol);
        Self { symbol, hash }
    }
}

/// One coded symbol (reference: `CodedSymbol`): the XOR-sum of the source
/// symbols mapped to this index, the XOR-sum of their checksums, and the
/// signed count of how many were added minus removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodedSymbol {
    pub symbol: Symbol,
    pub hash: u64,
    pub count: i64,
}

impl CodedSymbol {
    fn apply(&mut self, s: &HashedSymbol, direction: i64) {
        for (a, b) in self.symbol.iter_mut().zip(s.symbol.iter()) {
            *a ^= b;
        }
        self.hash ^= s.hash;
        self.count += direction;
    }

    /// Fixed wire encoding (D100): symbol ‖ hash LE ‖ count LE.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; CODED_SYMBOL_LEN] {
        let mut out = [0u8; CODED_SYMBOL_LEN];
        out[..32].copy_from_slice(&self.symbol);
        out[32..40].copy_from_slice(&self.hash.to_le_bytes());
        out[40..48].copy_from_slice(&self.count.to_le_bytes());
        out
    }

    #[must_use]
    pub fn from_bytes(bytes: &[u8; CODED_SYMBOL_LEN]) -> Self {
        let mut symbol = [0u8; 32];
        symbol.copy_from_slice(&bytes[..32]);
        let hash = u64::from_le_bytes(bytes[32..40].try_into().expect("8 bytes"));
        let count = i64::from_le_bytes(bytes[40..48].try_into().expect("8 bytes"));
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
#[derive(Clone, Copy, Debug)]
struct RandomMapping {
    prng: u64,
    last_index: u64,
}

impl RandomMapping {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn next_index(&mut self) -> u64 {
        self.prng = self.prng.wrapping_mul(0xda94_2042_e4dd_58b5);
        let r = self.prng;
        self.last_index += (((self.last_index as f64) + 1.5)
            * (((1u64 << 32) as f64) / ((r as f64) + 1.0).sqrt() - 1.0))
            .ceil() as u64;
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
#[derive(Default)]
struct CodingWindow {
    symbols: Vec<HashedSymbol>,
    mappings: Vec<RandomMapping>,
    queue: Vec<SymbolMapping>,
    next_idx: u64,
}

impl CodingWindow {
    fn add_hashed_symbol(&mut self, t: HashedSymbol) {
        self.add_hashed_symbol_with_mapping(
            t,
            RandomMapping {
                prng: t.hash,
                last_index: 0,
            },
        );
    }

    fn add_hashed_symbol_with_mapping(&mut self, t: HashedSymbol, m: RandomMapping) {
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
    fn apply_window(&mut self, mut cw: CodedSymbol, direction: i64) -> CodedSymbol {
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

/// Incremental encoder: seed it with a set, then produce the set's coded
/// symbol sequence one at a time, forever. Adding symbols after the first
/// `produce_next_coded_symbol` is a logic error (the emitted prefix would
/// not include them), as in the reference.
#[derive(Default)]
pub struct Encoder {
    window: CodingWindow,
}

impl Encoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_symbol(&mut self, s: Symbol) {
        self.window.add_hashed_symbol(HashedSymbol::new(s));
    }

    pub fn produce_next_coded_symbol(&mut self) -> CodedSymbol {
        self.window.apply_window(CodedSymbol::default(), ADD)
    }
}

/// Streaming decoder: knows the local set B (the prior, fed via
/// [`Decoder::add_symbol`] before any coded symbol arrives), consumes the
/// remote set A's coded symbols in order, and peels until the symmetric
/// difference is recovered — [`Decoder::remote`] is A∖B (what to fetch),
/// [`Decoder::local`] is B∖A (never leaves this process; the D100
/// asymmetric reveal).
#[derive(Default)]
pub struct Decoder {
    /// Coded symbols received so far, mutated as decoded symbols peel off.
    cs: Vec<CodedSymbol>,
    /// Recovered symbols exclusive to the decoder (B∖A).
    local: CodingWindow,
    /// The decoder's own set (the prior).
    window: CodingWindow,
    /// Recovered symbols exclusive to the encoder (A∖B).
    remote: CodingWindow,
    /// Indices of coded symbols currently decodable (degree ±1 with a
    /// matching checksum, or degree 0 with a zero checksum-sum).
    decodable: Vec<usize>,
    decoded: usize,
}

impl Decoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True iff every coded symbol received so far has been decoded —
    /// at which point `remote`/`local` hold the full symmetric difference.
    #[must_use]
    pub fn decoded(&self) -> bool {
        self.decoded == self.cs.len()
    }

    /// Symbols present at the encoder but not here (A∖B).
    pub fn remote(&self) -> impl Iterator<Item = &Symbol> {
        self.remote.symbols.iter().map(|s| &s.symbol)
    }

    /// Symbols present here but not at the encoder (B∖A).
    pub fn local(&self) -> impl Iterator<Item = &Symbol> {
        self.local.symbols.iter().map(|s| &s.symbol)
    }

    /// Add one symbol of the local set. Must precede all coded symbols.
    pub fn add_symbol(&mut self, s: Symbol) {
        self.window.add_hashed_symbol(HashedSymbol::new(s));
    }

    /// Consume the next coded symbol of the remote sequence, in order.
    pub fn add_coded_symbol(&mut self, c: CodedSymbol) {
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
    fn apply_new_symbol(&mut self, t: &HashedSymbol, direction: i64) -> RandomMapping {
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
                // The reference panics here (its invariant says this is
                // unreachable). Coded symbols come off the wire, so we
                // refuse instead of aborting the daemon: skipping leaves
                // the symbol undecoded, `decoded()` stays false, and the
                // initiator gives up by budget — a failed reconcile, not
                // a peer-triggered crash.
                _ => debug_assert!(false, "decodable coded symbol with degree {}", c.count),
            }
            didx += 1;
        }
        self.decodable.clear();
    }
}

#[cfg(test)]
mod tests {
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
            out[(j as usize) * 8..(j as usize + 1) * 8]
                .copy_from_slice(&splitmix64(i * 4 + j).to_le_bytes());
        }
        out
    }

    /// Drive one full reconciliation in memory; returns (symbols used,
    /// remote diff, local diff), the loop the reference's example runs.
    fn reconcile(alice: &[Symbol], bob: &[Symbol]) -> (usize, Vec<Symbol>, Vec<Symbol>) {
        let mut enc = Encoder::new();
        for s in alice {
            enc.add_symbol(*s);
        }
        let mut dec = Decoder::new();
        for s in bob {
            dec.add_symbol(*s);
        }
        let mut used = 0;
        loop {
            dec.add_coded_symbol(enc.produce_next_coded_symbol());
            used += 1;
            dec.try_decode();
            if dec.decoded() {
                break;
            }
            assert!(used < 100_000, "reconciliation did not converge");
        }
        let mut remote: Vec<Symbol> = dec.remote().copied().collect();
        let mut local: Vec<Symbol> = dec.local().copied().collect();
        remote.sort_unstable();
        local.sort_unstable();
        (used, remote, local)
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
            let mut enc = Encoder::new();
            for i in 0..n {
                enc.add_symbol(test_symbol(i));
            }
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

    #[test]
    fn coded_symbol_wire_roundtrip() {
        let mut enc = Encoder::new();
        for i in 0..10 {
            enc.add_symbol(test_symbol(i));
        }
        for _ in 0..100 {
            let c = enc.produce_next_coded_symbol();
            assert_eq!(CodedSymbol::from_bytes(&c.to_bytes()), c);
        }
    }
}
