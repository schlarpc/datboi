# breakbulk configuration: every Rust build in the repo (host workspace,
# the ten wasm component workspaces, the emu lane). One derivation per
# Cargo unit; the non-Rust inputs (web dist, magic.mgc, the wasi C
# toolchain, out-of-tree C sources) arrive through `datboi` (flake.nix
# specialArgs) and reach ONLY the units of the package that reads them.
{ pkgs, lib, root, repo, breakbulk, datboi, ... }:
let
  components = import ./components.nix { inherit lib; };
  inherit (components) wasmTriple;
  native = pkgs.stdenv.hostPlatform.rust.rustcTarget;

  toolchain = {
    rustc = datboi.rustToolchain;
    cargo = datboi.rustToolchain;
  };

  # D54 stamp, as a post-link stage on the wasm32 core module. Replaces
  # the preset's `componentize` (whose metadata is eval-time only, and
  # the revision here is computed in-derivation) and drops `wasmOpt`
  # (component bytes are a reproducibility contract; the crane build
  # never ran wasm-opt). `revision` carries the GIT TREE HASHES of the
  # crate and of its lane's guest crate (D89), from raw flake paths — a
  # flake path is exactly the git-tracked content, so the hashes equal
  # `git rev-parse <commit>:crates/<crate>` verbatim.
  stampStage = crate:
    let
      crateToml = builtins.fromTOML (builtins.readFile (root + "/crates/${crate}/Cargo.toml"));
      moduleName = builtins.replaceStrings [ "-" ] [ "_" ] crate;
      # Stamped name stays `datboi:xf-*` / `datboi:ex-*` — the `datboi-`
      # crate-name prefix would double the namespace.
      stampName = lib.removePrefix "datboi-" crate;
      crateSrc = root + "/crates/${crate}";
      guestSrc = root + "/crates/${components.guestCrateFor crate}";
    in
    {
      # After install: it sees pkg/<module>.wasm (wasm's installed
      # layout) and writes lib/<module>.wasm, the layout D66 embeds
      # from and the dev build.rs fallback expects.
      after = "install";
      run = { input, pkgs, ... }:
        pkgs.runCommand "${crate}-stamped"
          { nativeBuildInputs = [ pkgs.wasm-tools pkgs.gitMinimal ]; }
          ''
            set -euo pipefail
            export GIT_DIR="$TMPDIR/rev-git"
            git init -q "$GIT_DIR"
            export GIT_INDEX_FILE="$TMPDIR/rev-index"
            git --work-tree=${crateSrc} add -A
            tree=$(git write-tree)
            export GIT_INDEX_FILE="$TMPDIR/rev-index-guest"
            git --work-tree=${guestSrc} add -A
            guest=$(git write-tree)
            mkdir -p $out/lib
            wasm-tools component new ${input}/pkg/${moduleName}.wasm -o stamped-input.wasm
            wasm-tools metadata add stamped-input.wasm \
              --name "datboi:${stampName}" \
              --description ${lib.escapeShellArg crateToml.package.description} \
              --authors ${lib.escapeShellArg (builtins.head crateToml.package.authors)} \
              --licenses ${lib.escapeShellArg crateToml.package.license} \
              --source "https://github.com/schlarpc/datboi/tree/main/crates/${crate}" \
              --revision "tree:$tree;guest:$guest" \
              -o "$out/lib/${moduleName}.wasm"
          '';
    };

  componentWorkspace = crate: {
    path = "crates/${crate}";
    inherit toolchain;
    targets = {
      # unknown-unknown, NOT wasip2: components import nothing (D5).
      ${wasmTriple} = {
        # Wasm artifacts are data; the host runs them under the
        # determinism gate. Unit tests run natively (below).
        tests = false;
        preset = breakbulk.presets.wasmComponent;
        stages = {
          wasmOpt = lib.mkForce null;
          componentize = lib.mkForce null;
          stamp = stampStage crate;
        };
      };
      # Component logic is target-independent: its unit tests run
      # natively (crane's `<crate>-test` checks).
      ${native} = { };
    };
    defaultTarget = wasmTriple;
  };
in
{
  workspaces = {
    # ---- host workspace (crates/) ----
    host = {
      path = ".";
      inherit toolchain;
      # Every test binary runs under nextest (.config/nextest.toml's
      # D106 hang guard applies), with the D62 fsck gate armed:
      # fsck.vfat must exist and MUST run. sqlite3: the D122 bless gate
      # walks a cache.db row back by hand.
      memberDefaults.test = {
        runner = "nextest";
        env.DATBOI_REQUIRE_FSCK = "1";
        path = [ pkgs.dosfstools pkgs.sqlite ];
      };
      # --workspace --all-targets -D warnings, as the crane gate ran it.
      variants.clippy = {
        mode = "clippy";
        clippyArgs = [ "--deny" "warnings" ];
      };
      fmt.enable = true;
    };

    # ---- browser emulator core (D84): nightly-only, wasm-bindgen ----
    datboi-emu-ds = {
      path = "crates/datboi-emu-ds";
      toolchain = {
        rustc = datboi.emuToolchain;
        cargo = datboi.emuToolchain;
      };
      targets.${wasmTriple} = {
        tests = false;
        preset = breakbulk.presets.wasmWeb;
        stages = {
          # The glue is version-locked to the CLI the crate pins exactly
          # (wasm-bindgen = "=0.2.121"), which is nixpkgs' wasm-bindgen-cli.
          bindgen.run = breakbulk.stageHelpers.bindgen {
            wasmBindgen = pkgs.wasm-bindgen-cli;
            extraArgs = [ "--no-typescript" ];
          };
          # crane never ran wasm-opt on the core; keep the bytes as built.
          wasmOpt = lib.mkForce null;
          # The core asset proper (descriptor.json + worker.js, the GPL
          # boundary) and the bare test page ride beside pkg/.
          assets = {
            after = "install";
            run = { input, pkgs, ... }:
              pkgs.runCommand "datboi-emu-ds-asset" { } ''
                mkdir -p $out
                cp -r ${input}/. $out/
                chmod -R u+w $out
                cp -r ${root + "/crates/datboi-emu-ds/asset"}/. $out/
                cp -r ${root + "/crates/datboi-emu-ds/test-page"}/. $out/
              '';
          };
        };
      };
    };
  } // lib.genAttrs components.names componentWorkspace;

  packages = {
    # libmagic for the `magic` crate (D79 blob sniff).
    magic-sys.inputs = p: [ p.file ];

    # datboi-api's only product is datboi-gen-openapi, a dev tool run
    # from a checkout (D69) whose job is to write openapi.json back into
    # the source tree it was built from — so it names that tree by
    # design (CARGO_MANIFEST_DIR). It never ships; the check is for
    # shipped outputs.
    datboi-api.checks.noSourceReferences = null;

    # WIT is read by path from the bindgen macros (../../wit).
    datboi-runtime.extraSources = [ "wit" ];
    datboi-guest-transform.extraSources = [ "wit" ];
    datboi-guest-extractor.extraSources = [ "wit" ];

    # D66/D67/D79/D84 embeds: only the build-script run and the
    # package's own units see them, so a web/ or component edit
    # rebuilds the embedding crate and its dependents, nothing else.
    datboi-runtime.forTarget.env.DATBOI_COMPONENTS_DIR = "${datboi.transforms}/lib";
    datboi-ingest.forTarget.env.DATBOI_COMPONENTS_DIR = "${datboi.transforms}/lib";
    datboi-server.forTarget.env = {
      DATBOI_WEB_DIST = "${datboi.web}";
      DATBOI_MAGIC_DB = "${datboi.magicdb}";
      DATBOI_EMU_DS = "${datboi.emuDs}";
    };

    # Cross-crate test fixtures (read with include_bytes!("../../…")).
    datboi-runtime.test.sourceScope = [
      "crates/datboi-ingest/tests/fixtures"
      "crates/datboi-xf-xgd1-prng/data"
    ];
    datboi-ingest.test.sourceScope = [ "crates/datboi-runtime/tests/fixtures" ];
    datboi-exec.test.sourceScope = [ "crates/datboi-runtime/tests/fixtures" ];

    # Extractors cross-compile vendored C/C++ to wasm32 in build.rs (D58);
    # the toolchain and the out-of-tree sources ride env on the wasm
    # target only (build.rs is a no-op natively).
    datboi-ex-unrar.target.${wasmTriple}.forTarget.env =
      datboi.wasiEnv // { DATBOI_UNRAR_SRC = "${datboi.unrarSrc}"; };
    datboi-ex-7z.target.${wasmTriple}.forTarget.env =
      datboi.wasiEnv // { DATBOI_LZMA_SRC = "${datboi.lzmaSdk}"; };
  };
}
