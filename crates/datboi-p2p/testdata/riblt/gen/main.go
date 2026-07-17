// Golden-vector generator for the datboi riblt port (D100).
//
// This program drives the REFERENCE implementation of Practical Rateless
// Set Reconciliation (github.com/yangl1996/riblt, the SIGCOMM 2024
// artifact) with datboi's symbol type — [32]byte, SipHash-2-4 checksum
// under the datboi protocol keys — and writes golden vectors into
// ../golden/. The Rust module (src/riblt.rs) replays the same cases and
// must match byte-for-byte; that differential is the port's correctness
// proof against the paper's artifact.
//
// Run manually from this directory to (re)generate:
//
//	go mod tidy && go run .
//
// Dependencies are hash-pinned by go.sum; the generator needs network to
// fetch them, the committed goldens do not. Regeneration is only ever
// needed if the cases change — the protocol constants may not change
// (wire break, D100).
package main

import (
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/dchest/siphash"
	"github.com/yangl1996/riblt"
)

// The datboi protocol keys (riblt.rs SIPHASH_K0/K1): LE bytes of
// "datboi/r" and "iblt/1\0\0".
const (
	k0 = 0x722f696f62746164
	k1 = 0x0000312f746c6269
)

// Sym mirrors datboi's riblt::Symbol: a bare 32-byte hash.
type Sym [32]byte

func (s Sym) XOR(t2 Sym) Sym {
	for i := range s {
		s[i] ^= t2[i]
	}
	return s
}

func (s Sym) Hash() uint64 {
	return siphash.Hash(k0, k1, s[:])
}

// splitmix64 matches the Rust tests' deterministic symbol constructor.
func splitmix64(x uint64) uint64 {
	z := x + 0x9E3779B97F4B9C15
	z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
	z = (z ^ (z >> 27)) * 0x94D049BB133111EB
	return z ^ (z >> 31)
}

func testSymbol(i uint64) Sym {
	var out Sym
	for j := uint64(0); j < 4; j++ {
		binary.LittleEndian.PutUint64(out[j*8:], splitmix64(i*4+j))
	}
	return out
}

func codedBytes(c riblt.CodedSymbol[Sym]) []byte {
	buf := make([]byte, 48)
	copy(buf[:32], c.Symbol[:])
	binary.LittleEndian.PutUint64(buf[32:40], c.Hash)
	binary.LittleEndian.PutUint64(buf[40:48], uint64(c.Count))
	return buf
}

// runDecode reconciles two ranges of test symbols through the reference
// and renders one golden "case" block (the line format cases.txt holds).
func runDecode(name string, aliceStart, aliceEnd, bobStart, bobEnd uint64) string {
	enc := &riblt.Encoder[Sym]{}
	for i := aliceStart; i < aliceEnd; i++ {
		enc.AddSymbol(testSymbol(i))
	}
	dec := &riblt.Decoder[Sym]{}
	for i := bobStart; i < bobEnd; i++ {
		dec.AddSymbol(testSymbol(i))
	}
	used := 0
	for {
		dec.AddCodedSymbol(enc.ProduceNextCodedSymbol())
		used++
		dec.TryDecode()
		if dec.Decoded() {
			break
		}
		if used >= 100000 {
			panic("did not converge: " + name)
		}
	}
	remote := []string{}
	for _, s := range dec.Remote() {
		remote = append(remote, hex.EncodeToString(s.Symbol[:]))
	}
	local := []string{}
	for _, s := range dec.Local() {
		local = append(local, hex.EncodeToString(s.Symbol[:]))
	}
	sort.Strings(remote)
	sort.Strings(local)
	var b strings.Builder
	fmt.Fprintf(&b, "case %s %d %d %d %d %d\n", name, aliceStart, aliceEnd, bobStart, bobEnd, used)
	for _, s := range remote {
		fmt.Fprintf(&b, "remote %s\n", s)
	}
	for _, s := range local {
		fmt.Fprintf(&b, "local %s\n", s)
	}
	b.WriteString("end\n")
	return b.String()
}

func main() {
	outDir := filepath.Join("..", "golden")
	if err := os.MkdirAll(outDir, 0o755); err != nil {
		panic(err)
	}

	// Encoder byte streams: first 256 coded symbols over {testSymbol(i) : i < N}.
	for _, n := range []uint64{1, 3, 100} {
		enc := &riblt.Encoder[Sym]{}
		for i := uint64(0); i < n; i++ {
			enc.AddSymbol(testSymbol(i))
		}
		var stream []byte
		for k := 0; k < 256; k++ {
			stream = append(stream, codedBytes(enc.ProduceNextCodedSymbol())...)
		}
		path := filepath.Join(outDir, fmt.Sprintf("encoder_%d.bin", n))
		if err := os.WriteFile(path, stream, 0o644); err != nil {
			panic(err)
		}
	}

	var b strings.Builder
	for _, i := range []uint64{0, 1, 2, 7, 1000} {
		s := testSymbol(i)
		fmt.Fprintf(&b, "hash %d %s %d\n", i, hex.EncodeToString(s[:]), s.Hash())
	}
	b.WriteString(runDecode("variant-pair", 0, 1000, 20, 1020))
	b.WriteString(runDecode("small-overlap", 0, 10, 5, 12))
	b.WriteString(runDecode("identical", 0, 500, 0, 500))
	b.WriteString(runDecode("empty-prior", 0, 100, 0, 0))
	b.WriteString(runDecode("empty-remote", 0, 0, 0, 50))

	if err := os.WriteFile(filepath.Join(outDir, "cases.txt"), []byte(b.String()), 0o644); err != nil {
		panic(err)
	}
	fmt.Println("golden vectors written to", outDir)
}
