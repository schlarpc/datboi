# Materialises the unrar C++ source the ex-unrar component compiles (D58), so
# the ~800K vendored tree no longer lives in the repo: a hash-pinned rarlab
# tarball plus this crate's own patch series. The result is handed to build.rs
# through DATBOI_UNRAR_SRC — the flake wires it (see `unrarSrcFor`); set it
# yourself to a prepared source tree for a non-Nix build.
#
# Identity note (D54): moving the bytes out doesn't weaken the "reproducible
# from this directory alone" contract — the crate now carries a *cryptographic
# commitment* to them (the tarball hash below) plus the exact datboi delta (the
# patches/), instead of the unpacked, modified source. The component's git
# tree hash still fully describes what it compiles.
#
# The patch series IS the datboi-owned delta, auditable as a diff rather than
# smeared invisibly through vendored C++:
#   0001 — guard __builtin_cpu_supports: the freestanding wasm target's clang
#          defines __GNUC__ but not __GLIBC__, so bare `#elif __GNUC__` would
#          emit x86 CPUID intrinsics that don't exist on wasm. (Mirrors the
#          upstream unrar_sys build fix; still required at 7.2.7.)
#   0002 — D58: the build is -fno-exceptions, so every `throw` becomes
#          datboi_wasm_trap() and every try/catch is removed. 7.2.7's window
#          allocator (largepage.hpp new_l) is made nothrow so OOM degrades to
#          the fragmented window instead of throwing, matching 7.1.0's
#          malloc/NULL behaviour.
{ fetchurl, applyPatches }:

applyPatches {
  name = "unrar-7.2.7-datboi-src";

  # rarlab prunes old source tarballs (7.1.0 is already a 404), so this pins
  # the current release by content hash. Bumping unrar = refresh url + hash
  # here and rebase the patch series onto the new tree.
  src = fetchurl {
    url = "https://www.rarlab.com/rar/unrarsrc-7.2.7.tar.gz";
    hash = "sha256-AdkDp9z0E8spJWltd5bkjjjUcfeb/n7zrSrr9sEtvv0=";
  };

  # Applied in order with `patch -p1` from the tarball's `unrar/` root; $out is
  # that patched source directory (the .cpp/.hpp sit at its top level).
  patches = [
    ./patches/0001-guard-cpu-supports-non-glibc.patch
    ./patches/0002-d58-exceptions-to-wasm-traps.patch
  ];
}
