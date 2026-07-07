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

      # cleanCargoSource drops non-Rust files; keep test fixtures (.xml, and
      # the committed .wasm component the determinism gate pins) and the WIT
      # ABI (.wit) in the build source.
      srcFor = system: root:
        let craneLib = craneLibFor system;
        in
        nixpkgs.lib.cleanSourceWith {
          src = root;
          filter = path: type:
            (builtins.match ".*\\.(xml|wit|wasm)$" path != null)
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

      # ---- transforms workspace (transforms/, wasm32-wasip2) ----

      wasmArgsFor = system:
        {
          src = srcFor system ./transforms;
          strictDeps = true;
          pname = "datboi-transforms";
          version = "0.1.0";
          # unknown-unknown, NOT wasip2: components must import nothing (D5
          # empty-import determinism contract), and wasip2's std wires WASI
          # shims into every component. Core modules are componentized with
          # `wasm-tools component new` in the install phase.
          CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
          # Wasm artifacts are data, not executables to test here; the host
          # runs them under the determinism gate instead.
          doCheck = false;
        };

      wasmDepsFor = system:
        let craneLib = craneLibFor system;
        in craneLib.buildDepsOnly (wasmArgsFor system);

      # Transforms built natively so their unit tests run under nextest
      # (transform logic is target-independent; wasm artifacts are checked
      # by building them, tested by the host determinism gate later).
      wasmHostTestArgsFor = system:
        {
          src = srcFor system ./transforms;
          strictDeps = true;
          pname = "datboi-transforms-host";
          version = "0.1.0";
        };

    in
    {
      packages = eachSystem (system:
        let
          craneLib = craneLibFor system;
          hostArgs = hostArgsFor system;
          wasmArgs = wasmArgsFor system;
        in
        {
          default = craneLib.buildPackage (hostArgs // {
            cargoArtifacts = hostDepsFor system;
            doCheck = false;
          });

          datboi = self.packages.${system}.default;

          transforms = craneLib.buildPackage (wasmArgs // {
            cargoArtifacts = wasmDepsFor system;
            nativeBuildInputs = [ (pkgsFor system).wasm-tools ];
            # Each cdylib is a core module; componentize it so the package
            # contents are real `datboi:transform` components — the
            # content-addressed artifacts recipes pin (D5/D6).
            installPhaseCommand = ''
              mkdir -p $out/lib
              for m in target/wasm32-unknown-unknown/release/*.wasm; do
                wasm-tools component new "$m" -o "$out/lib/$(basename "$m")"
              done
            '';
          });
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

          transforms-test =
            let wasmHostArgs = wasmHostTestArgsFor system;
            in
            craneLib.cargoNextest (wasmHostArgs // {
              cargoArtifacts = craneLib.buildDepsOnly wasmHostArgs;
              partitions = 1;
              partitionType = "count";
            });
        });

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
