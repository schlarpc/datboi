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
      # includes the wasm targets for the component crates.
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
            # The checked-in /v1 contract spec (D69): datboi-api's
            # staleness test include_str!s it, so hermetic builds need
            # it in the source.
            || (builtins.match ".*openapi\\.json$" path != null)
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

      # ---- components (crates/datboi-{xf,ex}-*, wasm32-unknown-unknown) ----
      #
      # Each component is a STANDALONE workspace with its own lockfile
      # (D54): sibling crates must not be able to perturb a component's
      # bytes through shared dependency resolution. The reproducibility
      # boundary is one crate directory plus the frozen ../../wit (D66
      # layout: crates/<crate> + ./wit).
      #
      # Two world families, distinguished by crate prefix (D58):
      # `datboi-xf-` = transform (@1 whole-buffer / @2 streaming),
      # `datboi-ex-` = extractor (seekable archive in → member streams +
      # metadata out). The build, stamp, and zero-import gate are
      # identical across both; extractors additionally cross-compile a
      # vendored C++ staticlib (see the wasiToolchainFor / datboi-ex-unrar
      # wiring below).

      wasmCrateNames = [
        "datboi-xf-cso"
        "datboi-xf-ecm"
        "datboi-xf-preflate"
        "datboi-xf-reference"
        "datboi-xf-reference-stream"
        "datboi-ex-unrar"
      ];

      # ---- C-to-wasm toolchain lane (D58) ----
      #
      # The extractor lane compiles vendored C++ (unrar) to freestanding
      # wasm32. wasi-sdk's clang++/wasm-ld + a wasm libc++/libc/builtins
      # provide the cross toolchain; the guest's build.rs consumes these
      # through DATBOI_WASI_* env vars. NOTE the component still imports
      # NOTHING (D5/D46) — the sysroot only supplies the C++ runtime the
      # determinism shim leaves undefined; wasi-libc's syscall-importing
      # objects are shut out by that shim (see crates/datboi-ex-unrar).
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
      isExtractor = crate: nixpkgs.lib.hasPrefix "datboi-ex-" crate;

      # The shared frozen WIT, staged so the guests' `../../wit/v2` path
      # resolves exactly as it does in the repo (crates/<crate>/../../wit):
      # nest the unpacked crate one level deeper and put wit at the top.
      witStageFor = system: ''
        mkdir -p "$NIX_BUILD_TOP/nest"
        mv "$NIX_BUILD_TOP/$sourceRoot" "$NIX_BUILD_TOP/nest/$sourceRoot"
        sourceRoot="nest/$sourceRoot"
        cp -r ${srcFor system ./wit} "$NIX_BUILD_TOP/wit"
        chmod -R u+w "$NIX_BUILD_TOP/wit"
      '';

      wasmCrateArgsFor = system: crate:
        let
          toolchain = wasiToolchainFor system;
        in
        {
          # Deliberately UNFILTERED (unlike the host's srcFor): a flake
          # path is exactly the git-tracked content, so the in-derivation
          # `git write-tree` reproduces `git rev-parse <commit>:crates/<crate>`
          # verbatim — cargo-filtering the source made the D54 stamp hash a
          # tree that exists nowhere in git history.
          src = ./crates + "/${crate}";
          strictDeps = true;
          pname = crate;
          version = "0.1.0";
          cargoLock = ./crates + "/${crate}/Cargo.lock";
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
      #   git rev-parse <commit>:crates/<crate>
      transformPackageFor = system: crate:
        let
          craneLib = craneLibFor system;
          pkgs = pkgsFor system;
          args = wasmCrateArgsFor system crate;
          crateToml = builtins.fromTOML (builtins.readFile (./crates + "/${crate}/Cargo.toml"));
          moduleName = builtins.replaceStrings [ "-" ] [ "_" ] crate;
          # Stamped name stays `datboi:xf-*` / `datboi:ex-*` — the
          # `datboi-` crate-name prefix would double the namespace.
          stampName = nixpkgs.lib.removePrefix "datboi-" crate;
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
              --name "datboi:${stampName}" \
              --description ${nixpkgs.lib.escapeShellArg crateToml.package.description} \
              --authors ${nixpkgs.lib.escapeShellArg (builtins.head crateToml.package.authors)} \
              --licenses ${nixpkgs.lib.escapeShellArg crateToml.package.license} \
              --source "https://github.com/schlarpc/datboi/tree/main/crates/${crate}" \
              --revision "tree:$tree" \
              -o "$out/lib/${moduleName}.wasm"
          '';
        });

      # All stamped components in one lib/ — the artifacts recipes pin
      # (D5/D6/D54) and the host embeds at build time (D66).
      transformsFor = system: (pkgsFor system).symlinkJoin {
        name = "datboi-transforms";
        paths = map (transformPackageFor system) wasmCrateNames;
      };

      # D66: host builds embed the nix-built components — the embedding
      # crates' build.rs re-exports this for `include_bytes!` (and falls
      # back to invoking `nix build .#transforms` itself in a dev
      # checkout, where this var is unset). Applied to the final
      # build/test/clippy args — NOT to buildDepsOnly, so a component
      # edit doesn't rebuild the dep cache.
      componentsEnvFor = system: {
        DATBOI_COMPONENTS_DIR = "${transformsFor system}/lib";
      };

      # ---- web UI (web/, D67) ----
      #
      # web/ is a standalone npm project with its own package-lock.json —
      # the lockfile boundary again (D54/D66): its derivation source is a
      # fileset over web/ plus EXACTLY ONE file from the rust side, the
      # checked-in OpenAPI spec (D69: `npm run generate` derives the TS
      # API types from it). Rust edits still never invalidate the web
      # build — only a change to the spec file itself does — and a web
      # edit never touches cargoArtifacts. Excluding install/build output
      # is belt-and-braces (untracked paths never reach a flake's source
      # anyway, hence maybeMissing).
      webSrc = nixpkgs.lib.fileset.toSource {
        root = ./.;
        fileset = nixpkgs.lib.fileset.unions [
          (nixpkgs.lib.fileset.difference ./web (nixpkgs.lib.fileset.unions [
            (nixpkgs.lib.fileset.maybeMissing ./web/node_modules)
            (nixpkgs.lib.fileset.maybeMissing ./web/dist)
            (nixpkgs.lib.fileset.maybeMissing ./web/.vite)
          ]))
          ./crates/datboi-api/openapi.json
        ];
      };

      webPackageJson = builtins.fromJSON (builtins.readFile ./web/package.json);

      # node_modules built purely from package-lock.json (rof-gui pattern,
      # docs/50-infra.md) — no npmDepsHash to churn on every lockfile edit.
      webNodeModulesFor = system: (pkgsFor system).importNpmLock.buildNodeModules {
        npmRoot = "${webSrc}/web";
        inherit (pkgsFor system) nodejs;
      };

      # The vite dist the datboi binary will embed and serve at / with an
      # SPA fallback (DATBOI_WEB_DIST, wired like DATBOI_COMPONENTS_DIR —
      # D66/D67). Tests live in checks.web-test, not here: the rust build
      # will depend on this derivation, and UI test churn must not sit on
      # that path.
      webFor = system:
        let pkgs = pkgsFor system;
        in
        pkgs.stdenv.mkDerivation {
          pname = "datboi-web";
          version = webPackageJson.version;
          src = webSrc;
          # webSrc roots at the repo so ../crates/datboi-api/openapi.json
          # resolves for `npm run generate`; the project itself is web/.
          sourceRoot = "source/web";
          nativeBuildInputs = [ pkgs.nodejs ];
          buildPhase = ''
            runHook preBuild
            ln -s ${webNodeModulesFor system}/node_modules node_modules
            npm run generate
            npm run build
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            cp -r dist $out
            runHook postInstall
          '';
        };

      # D67: the datboi binary embeds the nix-built web dist —
      # datboi-server's build.rs re-exports this for `include_dir!` (and
      # falls back to invoking `nix build .#web` itself in a dev
      # checkout, where this var is unset). Applied to the final
      # build/test/clippy args — NOT to buildDepsOnly, so a web edit
      # doesn't rebuild the dep cache (same placement as
      # componentsEnvFor, D66).
      webEnvFor = system: {
        DATBOI_WEB_DIST = "${webFor system}";
      };

    in
    {
      packages = eachSystem (system:
        let
          craneLib = craneLibFor system;
          hostArgs = hostArgsFor system;
        in
        {
          default = craneLib.buildPackage (hostArgs // componentsEnvFor system // webEnvFor system // {
            cargoArtifacts = hostDepsFor system;
            doCheck = false;
          });

          datboi = self.packages.${system}.default;

          transforms = transformsFor system;

          web = webFor system;
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
          web = self.packages.${system}.web;

          # svelte-check + vitest over the same fileset source. Generated
          # files first, mirroring the dev flow: `npm run generate` derives
          # the TS API types from the checked-in OpenAPI spec (D69,
          # gitignored like the loaders), and `npm run extract` writes the
          # wuchale loader modules svelte-check needs on disk to resolve
          # App.svelte's loader import (vitest regenerates them itself via
          # the vite plugin; both steps are deterministic and offline).
          web-test =
            let pkgs = pkgsFor system;
            in
            pkgs.stdenv.mkDerivation {
              pname = "datboi-web-test";
              version = webPackageJson.version;
              src = webSrc;
              sourceRoot = "source/web";
              nativeBuildInputs = [ pkgs.nodejs ];
              buildPhase = ''
                runHook preBuild
                ln -s ${webNodeModulesFor system}/node_modules node_modules
                npm run generate
                npm run extract
                npm run check
                npm test
                runHook postBuild
              '';
              installPhase = "touch $out";
            };

          clippy = craneLib.cargoClippy (hostArgs // componentsEnvFor system // webEnvFor system // {
            cargoArtifacts = hostArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          fmt = craneLib.cargoFmt {
            src = hostArgs.src;
          };

          test = craneLib.cargoNextest (hostArgs // componentsEnvFor system // webEnvFor system // {
            cargoArtifacts = hostArtifacts;
            partitions = 1;
            partitionType = "count";
            # The D62 fsck-in-CI gate: fsck.vfat must exist and MUST run
            # (the test skips gracefully outside nix; CI never skips).
            nativeCheckInputs = [ (pkgsFor system).dosfstools ];
            DATBOI_REQUIRE_FSCK = "1";
          });

        } // nixpkgs.lib.listToAttrs (map
          # Component unit tests run natively per crate (logic is
          # target-independent; wasm artifacts are exercised by the host
          # determinism/extractor gates). `--no-tests=pass` because an
          # extractor's logic can be entirely wasm-guest (ex-unrar's is C++
          # under the sandbox; its conformance lives in datboi-runtime), so
          # a crate legitimately has zero host-native tests.
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
                cargoNextestExtraArgs = "--no-tests=pass";
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
              # FAT32 image synthesis gate (D62 fsck-in-CI)
              pkgs.dosfstools
              nix-direnv.packages.${system}.default
              # web/ toolchain (D67): the link hook symlinks the store-built
              # node_modules into web/ on shell entry (npmRoot below, rof-gui
              # pattern). web/.npmrc has package-lock-only=true, so
              # `npm install` only edits the lockfile; re-enter the shell to
              # realize new deps.
              pkgs.nodejs
              pkgs.importNpmLock.hooks.linkNodeModulesHook
            ];

            npmDeps = webNodeModulesFor system;
            # Relative to where the shell is entered — the repo root (direnv).
            npmRoot = "web";

            RUST_BACKTRACE = "1";
            RUST_LOG = "debug";
            # DATBOI_COMPONENTS_DIR / DATBOI_WEB_DIST are deliberately
            # NOT set here (D66/D67): in a dev checkout the embedding
            # crates' build.rs runs `nix build .#transforms` / `.#web`
            # itself and watches the sources, so edits propagate on the
            # next cargo build with no shell reload. Hermetic builds
            # (crane) set the vars.
          };
        });

      lib = {
        inherit nix-direnv;
      };
    };
}
