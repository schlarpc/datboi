# Repo structure, nix, daemon conventions

*From research pass R4, modeled on schlarpc/rust-flake (crane + rust-overlay
+ toolchain.toml as source of truth + checks) and schlarpc/rof-gui
(importNpmLock + vite in mkDerivation).*

## Layout: single flake, no subflakes, no flake-parts

Subflakes cost lockfile duplication, `path:`-input staleness, and broken
`follows`; flake-parts buys modularity we don't need at this output count
and reads unlike the user's other repos. If outputs explode, split into
`nix/*.nix` imported by one flake.

```
datboi/
├── flake.nix                  # crane + rust-overlay + importNpmLock
├── rust-toolchain.toml        # stable + wasm32-unknown-unknown target (D42)
├── Cargo.toml                 # host workspace
├── crates/
│   ├── datboi-core/           # CAS, recipes, hashing, typestate domain model
│   ├── datboi-formats/        # dat parsers (logiqx, clrmamepro, MAME)
│   ├── datboi-store-{fs,s3,http,iroh}/
│   ├── datboi-index/          # two-file SQLite layer (65-schema.md) — split
│   │                          # from core to keep core dependency-light
│   ├── datboi-runtime/        # wasmtime host, limits
│   ├── datboi-server/         # axum daemon
│   ├── datboi/                # CLI (client subcommands + serve)
│   ├── datboi-xf-*/           # wasm transform components — each its OWN
│   │                          # standalone workspace + lockfile (D54/D66)
│   └── datboi-ex-*/           # wasm extractor components (D58), same shape
├── wit/                       # datboi:{transform,extractor} WIT worlds —
│                              # the versioned, frozen ABI
├── web/                       # vite + TS (rof-gui pattern)
└── docs/
```

The component crates are excluded from the host workspace: targets and
profiles differ (host vs wasm32-unknown-unknown — not wasip2, whose std
drags WASI imports into every component; see D42), and each component
keeps its own lockfile so sibling crates can't perturb its bytes through
shared dependency resolution (D54). The daemon embeds the built
components via `include_bytes!` of the nix store paths
(`DATBOI_COMPONENTS_DIR`, D66) — nix makes "transforms are
content-addressed artifacts" nearly literal; in a dev checkout the
embedding crates' build.rs invokes `nix build .#transforms` itself, so a
component edit lands on the next cargo build. Shared API types: a
`datboi-api` crate generates TS; WIT plays that role for the wasm
boundary.

## Daemon conventions

12-factor: axum + tokio; config strictly env (layered via figment or
clap-env); `tracing` structured JSON logs to stdout; single process; health
endpoint; no pidfiles. Server keypair doubles as iroh identity.

## Auth options (survey — decision pending)

| Option | Fit |
|---|---|
| Invite tokens → local accounts + sessions | Best default; zero external deps (Jellyfin model) |
| Passkeys (webauthn-rs) | Great UX atop invites; needs a stable origin |
| OIDC | Optional integration for IdP-running self-hosters; never the only path |
| Reverse-proxy header trust | Cheap homelab compatibility mode |
| iroh NodeId | The daemon↔daemon plane; separate from human auth |
