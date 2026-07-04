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

      hostArgsFor = system:
        let craneLib = craneLibFor system;
        in
        {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
          pname = "datboi";
          version = "0.1.0";
        };

      hostDepsFor = system:
        let craneLib = craneLibFor system;
        in craneLib.buildDepsOnly (hostArgsFor system);

      # ---- transforms workspace (transforms/, wasm32-wasip2) ----

      wasmArgsFor = system:
        let craneLib = craneLibFor system;
        in
        {
          src = craneLib.cleanCargoSource ./transforms;
          strictDeps = true;
          pname = "datboi-transforms";
          version = "0.1.0";
          CARGO_BUILD_TARGET = "wasm32-wasip2";
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
        let craneLib = craneLibFor system;
        in
        {
          src = craneLib.cleanCargoSource ./transforms;
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
            # cdylib outputs; install the .wasm files as package contents.
            installPhaseCommand = ''
              mkdir -p $out/lib
              find target/wasm32-wasip2 -name '*.wasm' -exec cp {} $out/lib/ \;
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
