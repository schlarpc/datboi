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
├── rust-toolchain.toml        # stable + wasm32-wasip2 target
├── Cargo.toml                 # host workspace
├── crates/
│   ├── datboi-core/           # CAS, recipes, hashing, typestate domain model
│   ├── datboi-formats/        # dat parsers (logiqx, clrmamepro, MAME)
│   ├── datboi-store-{fs,s3,http,iroh}/
│   ├── datboi-index/          # two-file SQLite layer (65-schema.md) — split
│   │                          # from core to keep core dependency-light
│   ├── datboi-runtime/        # wasmtime host, limits
│   ├── datboi-server/         # axum daemon
│   └── datboi/                # CLI (client subcommands + serve)
├── transforms/                # SEPARATE cargo workspace (wasm target/profile)
│   ├── wit/                   # datboi:transform WIT world — the versioned ABI
│   └── xf-*/                  # one crate per transform
├── web/                       # vite + TS (rof-gui pattern)
└── docs/
```

Two cargo workspaces because targets/profiles differ (host vs
wasm32-wasip2) and wasm crates shouldn't pollute the host dep cache. Each
transform is a `packages.xf-*` flake output; the daemon embeds builtin
transforms via `include_bytes!` of those store paths — nix makes
"transforms are content-addressed artifacts" nearly literal. Shared API
types: a `datboi-api` crate generates TS; WIT plays that role for the wasm
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
