# Materialises the 7-Zip C decoder source the ex-7z component compiles
# (D110), following the ex-unrar out-of-tree pattern (D66): a hash-pinned
# 7-zip.org source tarball, reduced to its `C/` tree (the LZMA SDK
# lineage — public domain, per the files' own headers). The result is
# handed to build.rs through DATBOI_LZMA_SRC — the flake wires it; set it
# yourself to an unpacked `C/` directory for a non-Nix build.
#
# Identity note (D54): the crate carries a cryptographic commitment to
# the decoder source (the tarball hash below); the component's git tree
# hash plus this pin fully describe what it compiles. No patches: the
# C decoder compiles for freestanding wasm32 as shipped.
#
# Bumping 7-Zip = refresh url + hash here; the version rides the file
# name, so a bump is visible in the diff.
{ fetchurl, runCommand }:

let
  src = fetchurl {
    url = "https://7-zip.org/a/7z2601-src.tar.xz";
    hash = "sha256-sjieDpMLL5o0jPD+fZhwpGSCqOwETuC99C4hNtsxw9Y=";
  };
in
runCommand "lzma-sdk-26.01-c" { } ''
  mkdir unpack
  tar -xf ${src} -C unpack
  mkdir "$out"
  cp -r unpack/C/. "$out"/
''
