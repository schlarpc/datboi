# NixOS module for the datboi daemon (D95, docs/infra.md §NixOS module).
#
# The option surface IS the daemon's 12-factor `DATBOI_*` surface (the
# same clap/env config as the CLI and the container image) dressed in
# NixOS-idiomatic camelCase: each friendly option owns exactly one
# `DATBOI_*` var, and `environment` is the escape hatch for any var not
# yet promoted to a first-class option. One config vocabulary end to end.
{ config, lib, pkgs, ... }:
let
  cfg = config.services.datboi;

  inherit (lib)
    mkEnableOption mkPackageOption mkOption mkIf types
    optionalAttrs literalExpression;

  # The daemon reads everything from the environment (`datboi serve` is
  # pure DATBOI_* config). Friendly options first, then the freeform
  # passthrough — so `environment` can override or extend, never fight.
  env =
    {
      DATBOI_STORE = cfg.store;
      DATBOI_DB_DIR = cfg.databaseDir;
      DATBOI_LISTEN = cfg.listenAddress;
    }
    // optionalAttrs (cfg.nfsListenAddress != null) {
      DATBOI_NFS_LISTEN = cfg.nfsListenAddress;
    }
    // optionalAttrs (cfg.detectorsDir != null) {
      DATBOI_DETECTORS = toString cfg.detectorsDir;
    }
    # Refinement is ON by default in the daemon; the env var is the
    # opt-OUT (DATBOI_NO_REFINE), so only emit it when disabled.
    // optionalAttrs (!cfg.refine) {
      DATBOI_NO_REFINE = "1";
    }
    // cfg.environment;

  # Port to open when openFirewall is set: the tail of host:port. IPv6
  # literals carry colons, so take everything after the LAST colon.
  listenPort = lib.toInt (lib.last (lib.splitString ":" cfg.listenAddress));
in
{
  options.services.datboi = {
    enable = mkEnableOption "datboi, dat/rom management on content-addressed storage";

    package = mkPackageOption pkgs "datboi" { };

    user = mkOption {
      type = types.str;
      default = "datboi";
      description = ''
        User the daemon runs as. When left at the default the module
        creates it; set it to an existing user to manage that yourself
        (the store and database dir must be writable by it).
      '';
    };

    group = mkOption {
      type = types.str;
      default = "datboi";
      description = "Group the daemon runs as (created when left at the default).";
    };

    store = mkOption {
      type = types.str;
      example = "/srv/datboi/store";
      description = ''
        Store root — the content-addressed data, meta, and tmp trees
        (`DATBOI_STORE`). May live on a network mount (NFS is fine here,
        D15); the unit orders itself after that mount automatically. The
        daemon creates the tree on first start. Required — there is no
        safe default for content storage.
      '';
    };

    databaseDir = mkOption {
      type = types.str;
      default = "/var/lib/datboi";
      description = ''
        Local state directory (`DATBOI_DB_DIR`): the SQLite databases and
        the instance identity key. MUST be daemon-local disk, never NFS
        (D15 — redb/SQLite-on-NFS is a corruption class). The identity
        key here is the one non-CAS secret: back it up out-of-band, it
        must survive a database wipe for recovery to trust its snapshots.
      '';
    };

    listenAddress = mkOption {
      type = types.str;
      default = "127.0.0.1:2352";
      example = "0.0.0.0:2352";
      description = ''
        HTTP/WebDAV listen address (`DATBOI_LISTEN`). Loopback
        connections are implicitly owner; binding wider means non-loopback
        requests require a session or bearer token (auth v1, D68). The
        owner bootstraps auth over loopback — no secret is configured here.
      '';
    };

    nfsListenAddress = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "0.0.0.0:2049";
      description = ''
        Also serve NFSv3 on this address (`DATBOI_NFS_LISTEN`; off when
        null). Consoles need a LAN bind, but NFS carries NO auth (D68) —
        only expose it on a trusted segment.
      '';
    };

    detectorsDir = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Directory of header-skipper detector XMLs (`DATBOI_DETECTORS`; optional).";
    };

    refine = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Ambient refinement (D71): a niced background worker analyzes fresh
        ingests and the corpus backlog continuously. Disabling sets
        `DATBOI_NO_REFINE`, turning sweeps back into a manual errand.
      '';
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Open the `listenAddress` port in the firewall. NFS (when enabled)
        is never opened automatically — it is unauthenticated (D68).
      '';
    };

    environment = mkOption {
      type = types.attrsOf types.str;
      default = { };
      example = literalExpression ''{ RUST_LOG = "info"; }'';
      description = ''
        Extra environment for the daemon — the escape hatch for any
        `DATBOI_*` var not surfaced as a first-class option, plus knobs
        like `RUST_LOG`. Merged last, so it wins over the options above.
      '';
    };
  };

  config = mkIf cfg.enable {
    users.users = mkIf (cfg.user == "datboi") {
      datboi = {
        isSystemUser = true;
        group = cfg.group;
        description = "datboi daemon";
        home = cfg.databaseDir;
      };
    };

    users.groups = mkIf (cfg.group == "datboi") {
      datboi = { };
    };

    networking.firewall = mkIf cfg.openFirewall {
      allowedTCPPorts = [ listenPort ];
    };

    # Both roots are managed here rather than via StateDirectory so the
    # store (which may be an arbitrary/NFS path) and the db dir are
    # created symmetrically with the right ownership — and so both are
    # writable under ProtectSystem=strict.
    systemd.tmpfiles.rules = [
      "d '${cfg.store}' 0750 ${cfg.user} ${cfg.group} - -"
      "d '${cfg.databaseDir}' 0700 ${cfg.user} ${cfg.group} - -"
    ];

    systemd.services.datboi = {
      description = "datboi — dat/rom management on content-addressed storage";
      documentation = [ "https://github.com/schlarpc/datboi" ];
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      # The store may be a network mount; do not start before it is up.
      unitConfig.RequiresMountsFor = [ cfg.store cfg.databaseDir ];

      environment = env;

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/datboi serve";
        User = cfg.user;
        Group = cfg.group;
        Restart = "on-failure";
        RestartSec = 5;
        UMask = "0077";

        ReadWritePaths = [ cfg.store cfg.databaseDir ];

        # Hardening. Kept deliberately compatible with the wasm runtime:
        # datboi runs transform/extractor components under wasmtime, whose
        # JIT maps executable pages — so NO MemoryDenyWriteExecute (it
        # would kill the runtime, docs/runtime.md).
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" ];
        CapabilityBoundingSet = "";
      };
    };
  };

  meta.maintainers = [ ];
}
