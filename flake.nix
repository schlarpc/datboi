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

    # NO nixpkgs.follows: its patched skopeo tracks the skopeo version
    # in its own pin; following ours breaks the patch (and forfeits any
    # cache hit).
    nix2container-turbo.url = "github:schlarpc/nix2container-turbo";

    nix-direnv = {
      url = "github:nix-community/nix-direnv";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, systems, rust-overlay, crane, nix-direnv, nix2container-turbo, git-hooks, ... }:
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

      # Formatting enforced at commit time (git-hooks.nix): the devshell's
      # shellHook installs a git pre-commit hook running the SAME pinned
      # rustfmt the CI fmt check uses, so an unformatted tree fails locally
      # instead of in CI. rustfmt only — it's the one formatter this repo
      # enforces (no prettier in web/, no nix formatter convention).
      preCommitFor = system:
        let rustToolchain = rustToolchainFor system;
        in
        git-hooks.lib.${system}.run {
          src = ./.;
          hooks.rustfmt = {
            enable = true;
            packageOverrides = {
              cargo = rustToolchain;
              rustfmt = rustToolchain;
            };
          };
        };

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
            (builtins.match ".*\\.(xml|wit|wasm|deflate|bin|rar|7z)$" path != null)
            # WIT package dependencies are symlinks named after the dep
            # package (wit/<lane>/v<n>/deps/streams → ../../../streams/v1,
            # D89) — extensionless, so the .wit match above misses them.
            || (builtins.match ".*/deps/[^/]+$" path != null)
            # ex-unrar vendors C++ (.cpp/.hpp) plus force-included compat
            # headers (.h) and the license/patch provenance (.txt); the
            # build.rs cross-compiles them (D58). ex-7z's glue is plain
            # C (.c) on the same lane (D110).
            || (builtins.match ".*\\.(c|cpp|hpp|h|txt|def|rc|vcxproj)$" path != null)
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
          # libmagic (the C library behind file(1)) for the `magic`
          # crate: the blob inspector's sniff (D79 headline fallback)
          # asks the real magic database instead of a hand-rolled
          # four-entry table.
          buildInputs = [ (pkgsFor system).file ];
        };

      hostDepsFor = system:
        let craneLib = craneLibFor system;
        in craneLib.buildDepsOnly (hostArgsFor system);

      # ---- components (crates/datboi-{xf,ex}-*, wasm32-unknown-unknown) ----
      #
      # Each component is a STANDALONE workspace with its own lockfile
      # (D54): sibling crates must not be able to perturb a component's
      # bytes through shared dependency resolution. The reproducibility
      # boundary is one crate directory plus the frozen ../../wit plus
      # the lane's guest crate ../datboi-guest-<lane> (D66 layout amended
      # by D89: the pregen-bindings crates are part of the ABI surface
      # and both tree hashes ride the stamp).
      #
      # Two lanes, distinguished by crate prefix (D58/D89):
      # `datboi-xf-` = datboi:transform (streaming; buffered authoring is
      # guest-crate sugar), `datboi-ex-` = datboi:extractor (seekable
      # containers in → member streams + metadata out). The build, stamp,
      # and zero-import gate are identical across both; extractors
      # additionally cross-compile a vendored C++ staticlib (see the
      # wasiToolchainFor / datboi-ex-unrar wiring below).

      wasmCrateNames = [
        "datboi-xf-cso"
        "datboi-xf-ecm"
        "datboi-xf-preflate"
        "datboi-xf-reference"
        "datboi-xf-reference-stream"
        "datboi-ex-unrar"
        "datboi-ex-7z"
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
          # The exact env the ex-* build.rs reads (CXX drives ex-unrar's
          # C++, CC drives ex-7z's plain C — same clang package).
          env = {
            DATBOI_WASI_CXX = "${cc}/bin/wasm32-unknown-wasi-clang++";
            DATBOI_WASI_CC = "${cc}/bin/wasm32-unknown-wasi-clang";
            DATBOI_WASI_WASMLD = "${bintools}/bin/wasm32-unknown-wasi-wasm-ld";
            DATBOI_WASI_LIBCXX_DIR = "${libcxx}/lib";
            DATBOI_WASI_LIBC_DIR = "${libc}/lib/wasm32-wasi";
            DATBOI_WASI_BUILTINS_DIR = "${builtins}/lib/wasi";
          };
        };

      # Extractor crates need the C-to-wasm toolchain in the build env;
      # transforms don't. Keyed by prefix.
      isExtractor = crate: nixpkgs.lib.hasPrefix "datboi-ex-" crate;

      # ex-unrar's C++ source lives out-of-tree (D66): a hash-pinned rarlab
      # tarball + this crate's patch series, fetched and patched here instead
      # of vendoring ~800K of unrar into the repo. build.rs consumes it via
      # DATBOI_UNRAR_SRC. The recipe is colocated with the crate that owns it.
      unrarSrcFor = system:
        (pkgsFor system).callPackage ./crates/datboi-ex-unrar/nix/unrar-src.nix { };

      # ex-7z's decoder source, same out-of-tree pattern (D110): the
      # hash-pinned 7-Zip tarball reduced to its public-domain C/ tree.
      # build.rs consumes it via DATBOI_LZMA_SRC.
      lzmaSdkFor = system:
        (pkgsFor system).callPackage ./crates/datboi-ex-7z/nix/lzma-sdk.nix { };

      # Which pregen-bindings crate a component lane consumes (D89,
      # docs/worlds.md §vending).
      guestCrateFor = crate:
        if isExtractor crate then "datboi-guest-extractor" else "datboi-guest-transform";

      # The shared ABI surface, staged so the guests' relative paths
      # resolve exactly as they do in the repo: nest the unpacked crate
      # one level deeper, put wit at the top (`../../wit/<lane>/v1`),
      # and stage the lane's guest crate beside the component
      # (`../datboi-guest-<lane>`). Raw flake paths on purpose — a flake
      # path is exactly the git-tracked content, so the staged trees
      # match `git rev-parse <commit>:wit` / `:crates/<guest>` verbatim.
      abiStageFor = system: crate: ''
        mkdir -p "$NIX_BUILD_TOP/nest"
        mv "$NIX_BUILD_TOP/$sourceRoot" "$NIX_BUILD_TOP/nest/$sourceRoot"
        sourceRoot="nest/$sourceRoot"
        cp -r ${./wit} "$NIX_BUILD_TOP/wit"
        cp -r ${./crates + "/${guestCrateFor crate}"} "$NIX_BUILD_TOP/nest/${guestCrateFor crate}"
        chmod -R u+w "$NIX_BUILD_TOP/wit" "$NIX_BUILD_TOP/nest/${guestCrateFor crate}"
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
          postUnpack = abiStageFor system crate;
        }
        # Extractors (ex-*) cross-compile a C++ staticlib in their build.rs;
        # hand it the wasi toolchain via env (D58). Transforms get nothing extra.
        // nixpkgs.lib.optionalAttrs (isExtractor crate) toolchain.env
        # ex-unrar additionally needs its out-of-tree unrar source (nix/unrar-src.nix).
        // nixpkgs.lib.optionalAttrs (crate == "datboi-ex-unrar") {
          DATBOI_UNRAR_SRC = "${unrarSrcFor system}";
        }
        # ex-7z likewise (nix/lzma-sdk.nix, D110).
        // nixpkgs.lib.optionalAttrs (crate == "datboi-ex-7z") {
          DATBOI_LZMA_SRC = "${lzmaSdkFor system}";
        };

      # One stamped component per crate (D54 attribution): identity
      # metadata rides IN the artifact as execution-inert custom sections,
      # and the loader refuses components without it. `revision` carries
      # the GIT TREE HASHES of the two source inputs (computed with
      # `git write-tree` over the build inputs — no .git needed):
      # `tree:` = the crate, `guest:` = the lane's pregen-bindings crate
      # (D89 — its source shapes component bytes too). Content-scoped,
      # so unrelated repo commits cannot churn component bytes, and
      # verifiable by anyone with git — no nix required:
      #   git rev-parse <commit>:crates/<crate>
      #   git rev-parse <commit>:crates/<datboi-guest-lane>
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
            # Revision = git tree hashes of the pristine sources (the
            # store copies, not the build dir — target/ must not leak in).
            export GIT_DIR="$TMPDIR/rev-git"
            git init -q "$GIT_DIR"
            export GIT_INDEX_FILE="$TMPDIR/rev-index"
            git --work-tree=${args.src} add -A
            tree=$(git write-tree)
            export GIT_INDEX_FILE="$TMPDIR/rev-index-guest"
            git --work-tree=${./crates + "/${guestCrateFor crate}"} add -A
            guest=$(git write-tree)
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
              --revision "tree:$tree;guest:$guest" \
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

      # ---- browser emulator cores (crates/datboi-emu-*, D84) ----
      #
      # The third wasm lane (docs/emulation.md): built like the
      # components (standalone workspace, own lockfile, wasm32 target) but
      # consumed like the web dist (a lazy-loaded static asset) — no WIT,
      # no componentization, no stamping, no determinism gate. dust-core
      # is nightly-only (core_intrinsics, portable_simd, ...), so the lane
      # carries its own pinned toolchain; the date tracks what dust's own
      # CI was green on around the pinned rev, and moves only deliberately.
      emuToolchainFor = system:
        let pkgs = pkgsFor system;
        in pkgs.rust-bin.nightly."2025-12-20".minimal.override {
          targets = [ "wasm32-unknown-unknown" ];
        };

      emuCraneLibFor = system:
        (crane.mkLib (pkgsFor system)).overrideToolchain (emuToolchainFor system);

      # wasm + JS glue via wasm-bindgen-cli (whose version the crate's
      # wasm-bindgen dep pins exactly — the glue is version-locked to the
      # CLI), plus the crate's bare test page at the output root:
      # `python3 -m http.server -d result/` and go.
      emuDsFor = system:
        let
          craneLib = emuCraneLibFor system;
          pkgs = pkgsFor system;
          args = {
            src = ./crates/datboi-emu-ds;
            strictDeps = true;
            pname = "datboi-emu-ds";
            version = "0.1.0";
            cargoLock = ./crates/datboi-emu-ds/Cargo.lock;
            CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
            doCheck = false;
          };
        in
        craneLib.buildPackage (args // {
          cargoArtifacts = craneLib.buildDepsOnly args;
          nativeBuildInputs = [ pkgs.wasm-bindgen-cli ];
          installPhaseCommand = ''
            mkdir -p $out
            wasm-bindgen --target web --no-typescript --out-dir $out/pkg \
              target/wasm32-unknown-unknown/release/datboi_emu_ds.wasm
            # The core asset proper: descriptor.json + worker.js next to
            # pkg/ (the host speaks only postMessage to worker.js — also
            # the GPL boundary). The test page rides along for the bare
            # serve-and-poke loop; the web UI won't ship it.
            cp -r asset/. $out/
            cp -r test-page/. $out/
          '';
        });

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
      # docs/infra.md) — no npmDepsHash to churn on every lockfile edit.
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

      # D84: the daemon embeds the emu core the same way (served under
      # /emu/nds/ by src/emu.rs) — same placement rules as webEnvFor.
      emuEnvFor = system: {
        DATBOI_EMU_DS = "${emuDsFor system}";
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

      # D79: the compiled magic database (file(1)'s magic.mgc) the blob
      # sniff embeds — copied out of nixpkgs' `file` so the output IS
      # the one file datboi-server's build.rs include_bytes!-es. Wired
      # like DATBOI_COMPONENTS_DIR / DATBOI_WEB_DIST; unlike those it
      # has no tree inputs, so only a nixpkgs bump moves it.
      magicDbFor = system:
        let pkgs = pkgsFor system;
        in pkgs.runCommand "datboi-magic-mgc" { } ''
          cp ${pkgs.file}/share/misc/magic.mgc $out
        '';

      magicEnvFor = system: {
        DATBOI_MAGIC_DB = "${magicDbFor system}";
      };

      # ---- WIT package distribution (D89, docs/worlds.md §publishing) ----
      #
      # Each lane version encoded into the binary package format wkg
      # publishes and consumers `wkg get`. Pure and deterministic — the
      # encoded package is content-addressed bytes; the impure OCI push
      # lives in publish-wit below.
      witPackagesFor = system:
        let pkgs = pkgsFor system;
        in pkgs.runCommand "datboi-wit-packages"
          { nativeBuildInputs = [ pkgs.wasm-tools ]; } ''
          mkdir -p $out
          for lane in ${./wit}/*; do
            for v in "$lane"/*; do
              # The package id (datboi:<lane>@<version>) names the file,
              # matching what `wkg wit build` would emit; wasm-tools does
              # the encoding because it resolves the deps/ symlinks
              # locally with no registry configuration.
              id=$(sed -n 's/^package \(datboi:[a-z-]*@[0-9.]*\);$/\1/p' "$v"/*.wit)
              wasm-tools component wit --wasm "$v" -o "$out/$id.wasm"
            done
          done
          ls $out
        '';

      # The publish gate (D89): every published version is immutable
      # forever — an existing tag is skipped (idempotent re-runs), never
      # overwritten. Refs follow the wkg OCI convention
      # (<base>/<package-namespace-derived-path>/<name>:<version>, the
      # WASI layout): the `datboi` package namespace is the path token,
      # so datboi:transform@1.0.0 → ghcr.io/schlarpc/datboi/transform:1.0.0,
      # beside the container image. Auth rides the ambient docker config
      # (the workflow's login-action); the registry override exists for
      # local smoke runs.
      publishWitFor = system:
        let pkgs = pkgsFor system;
        in pkgs.writeShellApplication {
          name = "publish-wit";
          runtimeInputs = [ pkgs.wkg pkgs.crane ];
          text = ''
            repo="''${DATBOI_WIT_REGISTRY:-ghcr.io/schlarpc/datboi}"
            for pkg in ${witPackagesFor system}/*.wasm; do
              base=$(basename "$pkg" .wasm)   # e.g. datboi:transform@1.0.0
              name=''${base#datboi:}; name=''${name%@*}
              version=''${base##*@}
              ref="$repo/$name:$version"
              if crane manifest "$ref" >/dev/null 2>&1; then
                echo "SKIP $ref (already published; versions are immutable, D89)"
              else
                wkg oci push "$ref" "$pkg"
                echo "PUBLISHED $ref"
              fi
            done
          '';
        };

      # ---- container image (nix2container-turbo) ----
      #
      # `docker run` starts the daemon (web UI on :2352); busybox rides
      # along so `docker run -it ... sh` / `docker exec` drop into a
      # shell with the datboi CLI on PATH. All daemon config is the
      # same clap/DATBOI_* surface as everywhere else — the image just
      # presets the 12-factor env. Two volumes because the two roots
      # have different placement rules (D15): the store may live on a
      # network mount, the DB dir MUST be daemon-local disk.
      containerFor = system:
        let
          pkgs = pkgsFor system;
          n2ct = nix2container-turbo.lib.${system};
          # /bin (datboi + busybox applets) and /etc/ssl (busybox wget
          # etc.; datboi's ureq bundles webpki roots and needs nothing).
          rootEnv = pkgs.buildEnv {
            name = "datboi-container-root";
            paths = [ self.packages.${system}.default pkgs.busybox pkgs.cacert ];
            pathsToLink = [ "/bin" "/etc" ];
          };
          # Volume mount points pre-created (the server, unlike the CLI,
          # does not create the db dir itself), plus /tmp and /root.
          skeleton = pkgs.runCommand "datboi-container-skeleton" { } ''
            mkdir -p $out/tmp $out/root $out/data/store $out/data/db
          '';
        in
        n2ct.buildImage {
          name = "ghcr.io/schlarpc/datboi";
          tag = "latest";
          copyToRoot = [ rootEnv skeleton ];
          perms = [{
            path = skeleton;
            regex = "/tmp";
            mode = "1777";
          }];
          config = {
            Cmd = [ "/bin/datboi" "serve" ];
            Env = [
              "PATH=/bin"
              "HOME=/root"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              # Same clap env plumbing as the CLI: flags > DATBOI_*.
              "DATBOI_STORE=/data/store"
              "DATBOI_DB_DIR=/data/db"
              # Loopback inside a container is unreachable; a wide bind
              # means auth-required, not open (D68).
              "DATBOI_LISTEN=0.0.0.0:2352"
            ];
            Volumes = {
              "/data/store" = { };
              "/data/db" = { };
            };
            ExposedPorts = { "2352/tcp" = { }; };
            WorkingDir = "/root";
          };
          # The turbo point: SOCI index pushed alongside via OCI
          # referrers, for lazy-pulling runtimes.
          push.soci.enable = true;
        };

    in
    {
      packages = eachSystem (system:
        let
          craneLib = craneLibFor system;
          hostArgs = hostArgsFor system;
        in
        {
          default = craneLib.buildPackage (hostArgs // componentsEnvFor system // webEnvFor system // magicEnvFor system // emuEnvFor system // {
            cargoArtifacts = hostDepsFor system;
            doCheck = false;
          });

          datboi = self.packages.${system}.default;

          transforms = transformsFor system;

          emu-ds = emuDsFor system;

          web = webFor system;

          magicdb = magicDbFor system;

          wit-packages = witPackagesFor system;

          publish-wit = publishWitFor system;
        } // nixpkgs.lib.optionalAttrs (nixpkgs.lib.hasSuffix "-linux" system) {
          container = containerFor system;
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
          # The emu lane (D84) has no host-side consumer yet (that's
          # spike milestones 2–3); building it in CI is what keeps the
          # nightly pin + upstream rev honest in the meantime.
          emu-ds = self.packages.${system}.emu-ds;
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

          clippy = craneLib.cargoClippy (hostArgs // componentsEnvFor system // webEnvFor system // magicEnvFor system // emuEnvFor system // {
            cargoArtifacts = hostArtifacts;
            # --workspace: default-members is just `cargo run` ergonomics
            # (root Cargo.toml); lints must cover every member.
            cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
          });

          fmt = craneLib.cargoFmt {
            src = hostArgs.src;
          };

          # The commit-time gate, also proven in CI so the hook config
          # can't rot (it runs the same hooks over the whole tree).
          pre-commit = preCommitFor system;

          test = craneLib.cargoNextest (hostArgs // componentsEnvFor system // webEnvFor system // magicEnvFor system // emuEnvFor system // {
            cargoArtifacts = hostArtifacts;
            partitions = 1;
            partitionType = "count";
            # --workspace: see the clippy note above.
            cargoNextestExtraArgs = "--workspace";
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
          wasmCrateNames)
        # D95: boot a VM, enable the module, prove the daemon comes up
        # and serves. Linux-only (nixosTest needs KVM); built on demand,
        # not on the default `nix build` path.
        // nixpkgs.lib.optionalAttrs (nixpkgs.lib.hasSuffix "-linux" system) {
          nixos-module = (pkgsFor system).testers.runNixOSTest {
            name = "datboi-module";
            nodes.machine = { pkgs, ... }: {
              imports = [ self.nixosModules.default ];
              environment.systemPackages = [ pkgs.curl ];
              services.datboi = {
                enable = true;
                store = "/srv/datboi/store";
              };
            };
            testScript = ''
              machine.wait_for_unit("datboi.service")
              machine.wait_for_open_port(2352)
              machine.succeed("curl -sf http://127.0.0.1:2352/healthz | grep -q ok")
              # Both roots exist, owned by the service user (D15 placement).
              machine.succeed("test -d /srv/datboi/store")
              machine.succeed("stat -c %U /srv/datboi/store | grep -qx datboi")
              machine.succeed("stat -c %U /var/lib/datboi | grep -qx datboi")
            '';
          };
        });

      devShells = eachSystem (system:
        let
          pkgs = pkgsFor system;
          rustToolchain = rustToolchainFor system;
          preCommit = preCommitFor system;
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];

            # Installs the pre-commit hook into .git/hooks on shell entry
            # (direnv), so formatting is enforced before every commit.
            shellHook = preCommit.shellHook;
            buildInputs = preCommit.enabledPackages;

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

      # Consume as a flake dep: add `overlays.default` for `pkgs.datboi`,
      # or import `nixosModules.default` and `services.datboi.enable = true`
      # (D95, docs/infra.md §NixOS module). The module defaults its
      # package to this flake's build for the host system, so it works
      # turnkey without the overlay; the overlay is for those who also
      # want the CLI in their system/user profile.
      overlays.default = final: _prev: {
        datboi = self.packages.${final.stdenv.hostPlatform.system}.default;
      };

      nixosModules.default = { pkgs, lib, ... }: {
        imports = [ ./nix/module.nix ];
        # mkDefault (1000) beats mkPackageOption's option-default (1500)
        # inside module.nix, and a user's plain assignment (100) still
        # beats this — so the flake build is the default, overridable.
        services.datboi.package =
          lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      };

      lib = {
        inherit nix-direnv;
      };
    };
}
