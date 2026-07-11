{
  description = "datboi — dat/rom management on content-addressed storage";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    systems.url = "github:nix-systems/default";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    nix-direnv = {
      url = "github:nix-community/nix-direnv";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, systems, rust-overlay, crane, nix-direnv, ... }:
    let
      eachSystem = nixpkgs.lib.genAttrs (import systems);

      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };

      # Toolchain pinned via rust-toolchain.toml (single source of truth);
      # includes the wasm32-wasip2 target for the transforms workspace.
      rustToolchainFor = system:
        let pkgs = pkgsFor system;
        in pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      craneLibFor = system:
        let
          pkgs = pkgsFor system;
          rustToolchain = rustToolchainFor system;
        in
        (crane.mkLib pkgs).overrideToolchain rustToolchain;

      # ---- host workspace (crates/) ----

      # cleanCargoSource drops non-Rust files; keep test fixtures (.xml,
      # the committed .wasm component the determinism gate pins, and the
      # preflate vectors .deflate/.bin) and the WIT ABI (.wit) in the
      # build source.
      srcFor = system: root:
        let craneLib = craneLibFor system;
        in
        nixpkgs.lib.cleanSourceWith {
          src = root;
          filter = path: type:
            (builtins.match ".*\\.(xml|wit|wasm|deflate|bin|rar)$" path != null)
            # ex-unrar vendors C++ (.cpp/.hpp) plus force-included compat
            # headers (.h) and the license/patch provenance (.txt); the
            # build.rs cross-compiles them (D58).
            || (builtins.match ".*\\.(cpp|hpp|h|txt|def|rc|vcxproj)$" path != null)
            || (craneLib.filterCargoSources path type);
          name = "source";
        };

      hostArgsFor = system:
        {
          src = srcFor system ./.;
          strictDeps = true;
          pname = "datboi";
          version = "0.1.0";
        };

      hostDepsFor = system:
        let craneLib = craneLibFor system;
        in craneLib.buildDepsOnly (hostArgsFor system);

      # ---- components (transforms/{xf,ex}-*, wasm32-unknown-unknown) ----
      #
      # Each component is a STANDALONE workspace with its own lockfile
      # (D54): sibling crates must not be able to perturb a component's
      # bytes through shared dependency resolution. The reproducibility
      # boundary is one crate directory plus the frozen ../wit.
      #
      # Two world families, distinguished by crate prefix (D58): `xf-` =
      # transform (@1 whole-buffer / @2 streaming), `ex-` = extractor
      # (seekable archive in → member streams + metadata out). The build,
      # stamp, and zero-import gate are identical across both; extractors
      # additionally cross-compile a vendored C++ staticlib (see the
      # wasiToolchainFor / ex-unrar wiring below).

      wasmCrateNames = [ "xf-cso" "xf-ecm" "xf-preflate" "xf-reference" "xf-reference-stream" "ex-unrar" ];

      # ---- C-to-wasm toolchain lane (D58) ----
      #
      # The extractor lane compiles vendored C++ (unrar) to freestanding
      # wasm32. wasi-sdk's clang++/wasm-ld + a wasm libc++/libc/builtins
      # provide the cross toolchain; the guest's build.rs consumes these
      # through DATBOI_WASI_* env vars. NOTE the component still imports
      # NOTHING (D5/D46) — the sysroot only supplies the C++ runtime the
      # determinism shim leaves undefined; wasi-libc's syscall-importing
      # objects are shut out by that shim (see transforms/ex-unrar).
      wasiToolchainFor = system:
        let
          pkgs = pkgsFor system;
          wasi = pkgs.pkgsCross.wasi32;
        in
        rec {
          cc = wasi.stdenv.cc;
          bintools = wasi.stdenv.cc.bintools.bintools;
          libcxx = wasi.llvmPackages.libcxx;
          libc = wasi.wasilibc;
          builtins = wasi.llvmPackages.compiler-rt;
          # The exact env the ex-* build.rs reads.
          env = {
            DATBOI_WASI_CXX = "${cc}/bin/wasm32-unknown-wasi-clang++";
            DATBOI_WASI_WASMLD = "${bintools}/bin/wasm32-unknown-wasi-wasm-ld";
            DATBOI_WASI_LIBCXX_DIR = "${libcxx}/lib";
            DATBOI_WASI_LIBC_DIR = "${libc}/lib/wasm32-wasi";
            DATBOI_WASI_BUILTINS_DIR = "${builtins}/lib/wasi";
          };
        };

      # Extractor crates need the C-to-wasm toolchain in the build env;
      # transforms don't. Keyed by prefix.
      isExtractor = crate: nixpkgs.lib.hasPrefix "ex-" crate;

      # The shared frozen WIT, staged next to the unpacked crate so the
      # guests' `../wit/v2` path resolves.
      witStageFor = system: ''
        cp -r ${srcFor system ./transforms/wit} $NIX_BUILD_TOP/wit
        chmod -R u+w $NIX_BUILD_TOP/wit
      '';

      wasmCrateArgsFor = system: crate:
        let
          toolchain = wasiToolchainFor system;
        in
        {
          src = srcFor system (./transforms + "/${crate}");
          strictDeps = true;
          pname = crate;
          version = "0.1.0";
          cargoLock = ./transforms + "/${crate}/Cargo.lock";
          # unknown-unknown, NOT wasip2: components must import nothing (D5
          # empty-import determinism contract), and wasip2's std wires WASI
          # shims into every component. Core modules are componentized with
          # `wasm-tools component new` in the install phase.
          CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
          # Wasm artifacts are data, not executables to test here; the host
          # runs them under the determinism gate instead.
          doCheck = false;
          postUnpack = witStageFor system;
        }
        # Extractors (ex-*) cross-compile a vendored C++ staticlib in their
        # build.rs; hand it the wasi toolchain via env (D58). Transforms get
        # nothing extra.
        // nixpkgs.lib.optionalAttrs (isExtractor crate) toolchain.env;

      # One stamped component per crate (D54 attribution): identity
      # metadata rides IN the artifact as execution-inert custom sections,
      # and the loader refuses components without it. `revision` is the
      # GIT TREE HASH of the crate source (computed with `git write-tree`
      # over the build inputs — no .git needed): content-scoped, so
      # unrelated repo commits cannot churn component bytes, and
      # verifiable by anyone with git — no nix required:
      #   git rev-parse <commit>:transforms/<crate>
      transformPackageFor = system: crate:
        let
          craneLib = craneLibFor system;
          pkgs = pkgsFor system;
          args = wasmCrateArgsFor system crate;
          crateToml = builtins.fromTOML (builtins.readFile (./transforms + "/${crate}/Cargo.toml"));
          moduleName = builtins.replaceStrings [ "-" ] [ "_" ] crate;
        in
        craneLib.buildPackage (args // {
          cargoArtifacts = craneLib.buildDepsOnly args;
          nativeBuildInputs = [ pkgs.wasm-tools pkgs.gitMinimal ];
          installPhaseCommand = ''
            # Revision = git tree hash of the pristine source (the store
            # copy, not the build dir — target/ must not leak in).
            export GIT_DIR="$TMPDIR/rev-git" GIT_INDEX_FILE="$TMPDIR/rev-index"
            git init -q "$GIT_DIR"
            git --work-tree=${args.src} add -A
            tree=$(git write-tree)
            mkdir -p $out/lib
            wasm-tools component new \
              target/wasm32-unknown-unknown/release/${moduleName}.wasm \
              -o stamped-input.wasm
            wasm-tools metadata add stamped-input.wasm \
              --name "datboi:${crate}" \
              --description ${nixpkgs.lib.escapeShellArg crateToml.package.description} \
              --authors ${nixpkgs.lib.escapeShellArg (builtins.head crateToml.package.authors)} \
              --licenses ${nixpkgs.lib.escapeShellArg crateToml.package.license} \
              --source "https://github.com/schlarpc/datboi/tree/main/transforms/${crate}" \
              --revision "tree:$tree" \
              -o "$out/lib/${moduleName}.wasm"
          '';
        });

    in
    {
      packages = eachSystem (system:
        let
          craneLib = craneLibFor system;
          hostArgs = hostArgsFor system;
        in
        {
          default = craneLib.buildPackage (hostArgs // {
            cargoArtifacts = hostDepsFor system;
            doCheck = false;
          });

          datboi = self.packages.${system}.default;

          # All stamped components in one lib/ — the artifacts recipes
          # pin (D5/D6/D54).
          transforms = (pkgsFor system).symlinkJoin {
            name = "datboi-transforms";
            paths = map (transformPackageFor system) wasmCrateNames;
          };
        });

      checks = eachSystem (system:
        let
          craneLib = craneLibFor system;
          hostArgs = hostArgsFor system;
          hostArtifacts = hostDepsFor system;
        in
        {
          build = self.packages.${system}.default;
          transforms = self.packages.${system}.transforms;

          clippy = craneLib.cargoClippy (hostArgs // {
            cargoArtifacts = hostArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          fmt = craneLib.cargoFmt {
            src = hostArgs.src;
          };

          test = craneLib.cargoNextest (hostArgs // {
            cargoArtifacts = hostArtifacts;
            partitions = 1;
            partitionType = "count";
          });

        } // nixpkgs.lib.listToAttrs (map
          # Transform unit tests run natively per crate (logic is
          # target-independent; wasm artifacts are exercised by the host
          # determinism gates).
          (crate:
            let
              args = builtins.removeAttrs (wasmCrateArgsFor system crate)
                [ "CARGO_BUILD_TARGET" "doCheck" ] // {
                pname = "${crate}-host";
              };
            in
            {
              name = "${crate}-test";
              value = craneLib.cargoNextest (args // {
                cargoArtifacts = craneLib.buildDepsOnly args;
                partitions = 1;
                partitionType = "count";
              });
            })
          wasmCrateNames));

      devShells = eachSystem (system:
        let
          pkgs = pkgsFor system;
          rustToolchain = rustToolchainFor system;
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];

            nativeBuildInputs = [
              rustToolchain
              pkgs.cargo-nextest
              pkgs.cargo-llvm-cov
              pkgs.bacon
              pkgs.cargo-edit
              pkgs.cargo-audit
              pkgs.cargo-expand
              # wasm component tooling (transforms lane)
              pkgs.wasm-tools
              pkgs.wasmtime
              nix-direnv.packages.${system}.default
            ];

            RUST_BACKTRACE = "1";
            RUST_LOG = "debug";
          };
        });

      lib = {
        inherit nix-direnv;
      };
    };
}
