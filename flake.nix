{
  description = "datboi — dat/rom management on content-addressed storage";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    systems.url = "github:nix-systems/default";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # Rust builds: one derivation per Cargo unit (nix/rust.nix). No
    # nixpkgs.follows: breakbulk's own tools (planner, unit builder)
    # then come from its pinned nixpkgs and stay put across our
    # nixpkgs bumps — a new unit builder would rebuild every unit.
    breakbulk.url = "github:schlarpc/breakbulk";

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

  outputs = { self, nixpkgs, systems, rust-overlay, breakbulk, nix-direnv, nix2container-turbo, git-hooks, ... }:
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

      # ---- Rust: every workspace through breakbulk (nix/rust.nix) ----
      #
      # One derivation per Cargo unit, planned by tracing our own cargo.
      # The host workspace, the ten standalone component workspaces
      # (D54: one crate = one lockfile, so one breakbulk workspace each)
      # and the nightly emu lane are all workspaces of one repository, so
      # identical units (same crate, features, profile, toolchain) are
      # shared between them. The non-Rust artifacts the Rust side embeds
      # ride `datboi` into the module, where each reaches only the
      # package whose build.rs reads it (D66/D67/D79/D84).
      components = import ./nix/components.nix { inherit (nixpkgs) lib; };

      repoFor = system:
        let
          pkgs = pkgsFor system;
          wasi = wasiToolchainFor system;
          repo = breakbulk.lib.mkRepository {
            inherit pkgs;
            root = self;
            modules = [ ./nix/rust.nix ];
            specialArgs.datboi = {
              rustToolchain = rustToolchainFor system;
              emuToolchain = emuToolchainFor system;
              wasiEnv = wasi.env;
              unrarSrc = unrarSrcFor system;
              lzmaSdk = lzmaSdkFor system;
              web = webFor system;
              magicdb = magicDbFor system;
              transforms = transformsOf pkgs repo;
              emuDs = repo.workspaces.datboi-emu-ds.packages.datboi-emu-ds;
            };
          };
        in
        repo;

      # Evaluated once per system: a repository's IFD plans are shared by
      # every output that names it.
      repos = eachSystem repoFor;

      # All stamped components in one lib/ — the artifacts recipes pin
      # (D5/D6/D54) and the host embeds at build time (D66): the D54
      # stamp stage's output, laid out by breakbulk's install stage.
      transformsOf = pkgs: repo: pkgs.symlinkJoin {
        name = "datboi-transforms";
        paths = map (components.productOf repo) components.names;
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
          repo = repos.${system};
        in
        {
          # The daemon + CLI binary (bin/datboi), with the components,
          # web dist, magic.mgc and emu core embedded at build time.
          default = repo.workspaces.host.packages.datboi;

          datboi = self.packages.${system}.default;

          transforms = transformsOf (pkgsFor system) repo;

          emu-ds = repo.workspaces.datboi-emu-ds.packages.datboi-emu-ds;

          web = webFor system;

          magicdb = magicDbFor system;

          wit-packages = witPackagesFor system;

          publish-wit = publishWitFor system;
        } // nixpkgs.lib.optionalAttrs (nixpkgs.lib.hasSuffix "-linux" system) {
          container = containerFor system;
        });

      checks = eachSystem (system:
        let
          repo = repos.${system};
          native = (pkgsFor system).stdenv.hostPlatform.rust.rustcTarget;
          # breakbulk's own gates: every product build of every
          # workspace, one test run per test binary (nextest per binary,
          # D62 fsck required), doctests, `host/<triple>/variant-clippy/
          # check` (--workspace --all-targets -D warnings) and
          # `host/fmt`. The component workspaces' native target exists
          # only for their unit tests (crane's `<crate>-test`), so its
          # products and their checks are left out.
          isNativeComponentProduct = name:
            builtins.any
              (crate: nixpkgs.lib.hasPrefix "${crate}/${native}/" name
                && builtins.match ".*/(test-[^/]*|doctest)" name == null)
              components.names;
          rustChecks = nixpkgs.lib.filterAttrs
            (name: _: !isNativeComponentProduct name)
            repo.checks;
        in
        rustChecks // {
          build = self.packages.${system}.default;
          transforms = self.packages.${system}.transforms;
          # The emu lane (D84) has no host-side consumer yet (that's
          # spike milestones 2–3); building it in CI is what keeps the
          # nightly pin + upstream rev honest in the meantime.
          emu-ds = self.packages.${system}.emu-ds;
          web = self.packages.${system}.web;
          # Short names for the two workspace-wide gates.
          clippy = repo.checks."host/${native}/variant-clippy/check";
          fmt = repo.checks."host/fmt";

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

          # The commit-time gate, also proven in CI so the hook config
          # can't rot (it runs the same hooks over the whole tree).
          pre-commit = preCommitFor system;

        }
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
          # Hand-written rather than breakbulk's workspace devShell: that
          # one merges every package's env into the shell, and the D66/D67
          # dev flow depends on DATBOI_* staying UNSET here (see below).
          default = pkgs.mkShell {
            # Installs the pre-commit hook into .git/hooks on shell entry
            # (direnv), so formatting is enforced before every commit.
            shellHook = preCommit.shellHook;
            # libmagic for the `magic` crate (D79), found by its build
            # script the same way the nix build's packages.magic-sys
            # input provides it.
            buildInputs = preCommit.enabledPackages ++ [ pkgs.file ];

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
              # the D122 bless gate's hand-walked cache.db row
              pkgs.sqlite
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
            # (nix/rust.nix) set the vars.
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
