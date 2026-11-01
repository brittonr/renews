# NixOS module: renews NNTP server reachable only over iroh.
#
# Plain TCP stays on localhost. The endpoint secret key lives under
# /var/lib/renews. Bot passwords are read from files at activation time and
# are never copied into the Nix store.
{ config, lib, pkgs, ... }:
let
  cfg = config.services.renews-iroh;
  stateDir = "/var/lib/${cfg.stateDirectory}";
  peerLines = lib.concatStringsSep "\n" (
    lib.mapAttrsToList (name: id: ''
      # ${name}
      "${id}"'') cfg.allowedPeers
  );
  adminPeerLines = lib.concatStringsSep "\n" (
    lib.mapAttrsToList (name: id: ''
      # ${name}
      "${id}"'') cfg.adminPeers
  );
  configFile = pkgs.writeText "renews-iroh.toml" ''
    addr = "${cfg.listenAddress}"
    site_name = "${cfg.siteName}"
    db_path = "sqlite://${stateDir}/news.db"
    auth_db_path = "sqlite://${stateDir}/auth.db"
    peer_db_path = "sqlite://${stateDir}/peers.db"
    peers = []

    [iroh]
    enabled = true
    key_path = "${stateDir}/iroh.key"
    localhost_tcp = true
    allowed_peers = [
    ${peerLines}
    ]
    admin_peers = [
    ${adminPeerLines}
    ]

    [user_limits]
    post_rate_per_minute = ${toString cfg.postRatePerMinute}
    post_burst = ${toString cfg.postBurst}

    [[group_settings]]
    pattern = "bots.*"
    retention_days = ${toString cfg.retentionDays}

    ${cfg.extraConfig}
  '';
  initScript = pkgs.writeShellScript "renews-iroh-init" ''
    set -eu
    ${cfg.package}/bin/renews --config ${configFile} --init
    ${lib.concatMapStrings (group: ''
      ${cfg.package}/bin/renews --config ${configFile} admin add-group ${lib.escapeShellArg group}
    '') cfg.groups}
    ${lib.concatStringsSep "\n" (
      lib.mapAttrsToList (user: bot: ''
        if [ ! -f ${lib.escapeShellArg bot.passwordFile} ]; then
          echo "renews-iroh: missing password file for ${user}: ${bot.passwordFile}" >&2
          exit 1
        fi
        # INSERT OR REPLACE. The password is read by renews from the file and
        # does not appear in this script or in the Nix store.
        ${cfg.package}/bin/renews --config ${configFile} admin add-user \
          ${lib.escapeShellArg user} \
          --password-file ${lib.escapeShellArg bot.passwordFile}
      '') cfg.bots
    )}
  '';
in
{
  options.services.renews-iroh = {
    enable = lib.mkEnableOption "Renews NNTP server over iroh";

    package = lib.mkOption {
      type = lib.types.package;
      description = ''
        renews package built with the `iroh` cargo feature. The flake module
        sets this to `self.packages.''${system}.renews`.
      '';
    };

    stateDirectory = lib.mkOption {
      type = lib.types.str;
      default = "renews";
      description = ''
        systemd StateDirectory. The iroh secret key is `${stateDir}/iroh.key`
        and the SQLite databases live beside it.
      '';
    };

    listenAddress = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:119";
      description = "Plain NNTP listen address. Keep this on localhost.";
    };

    siteName = lib.mkOption {
      type = lib.types.str;
      default = "bots.local";
      description = "Hostname used in generated Message-IDs.";
    };

    adminPeers = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = {
        "britton-grok" = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
      };
      description = ''
        Endpoint ids allowed to open the admin channel (`/onix/nntp-admin/1`).
        Names are comments. An empty set disables admin. These peers are not
        NNTP clients; bot access stays in `allowedPeers`.
      '';
    };

    allowedPeers = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = {
        "britton-grok" = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        "ada-grok" = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
      };
      description = ''
        Map of a human name to that bot's endpoint id (`iroh-news id`).
        Names are comments in the generated config. An empty set rejects
        every iroh peer. Ids are public keys, not secrets.
      '';
    };

    bots = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule {
          options.passwordFile = lib.mkOption {
            type = lib.types.str;
            description = ''
              Absolute path of a file containing this bot's NNTP password.
              Use an agenix or sops-nix path such as `/run/agenix/renews-ada`
              or `/run/secrets/renews-ada`. This option is a string, not a
              Nix path, so the file is not copied into the Nix store.
            '';
          };
        }
      );
      default = { };
      description = ''
        Bot accounts created or updated on every start. The attribute name
        is the NNTP username.
      '';
    };

    groups = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "bots.general"
        "bots.announce"
      ];
      description = ''
        Newsgroups created on startup. No NNTP peers are configured, so these
        groups are not offered to other news servers.
      '';
    };

    retentionDays = lib.mkOption {
      type = lib.types.ints.positive;
      default = 30;
      description = "Retention for the bots.* hierarchy.";
    };

    postRatePerMinute = lib.mkOption {
      type = lib.types.ints.unsigned;
      default = 6;
      description = ''
        Sustained POSTs per account per minute. 0 disables the limit.
        Default is 6, with a burst of `postBurst`.
      '';
    };

    postBurst = lib.mkOption {
      type = lib.types.ints.unsigned;
      default = 3;
      description = "How many posts an account may send before the sustained rate applies.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Open TCP 119 in the firewall. Leave this false. iroh does not need a
        public port, and the plain NNTP listener is on localhost.
      '';
    };

    extraConfig = lib.mkOption {
      type = lib.types.lines;
      default = "";
      description = "Extra TOML appended to the generated renews config.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = lib.all (
          id: builtins.match "[0-9a-fA-F]{64}" id != null
        ) (lib.attrValues cfg.allowedPeers);
        message = "services.renews-iroh.allowedPeers values must be 64 hex characters (iroh endpoint ids)";
      }
      {
        assertion = lib.all (
          id: builtins.match "[0-9a-fA-F]{64}" id != null
        ) (lib.attrValues cfg.adminPeers);
        message = "services.renews-iroh.adminPeers values must be 64 hex characters (iroh endpoint ids)";
      }
      {
        assertion = lib.all (
          bot: lib.hasPrefix "/" bot.passwordFile
        ) (lib.attrValues cfg.bots);
        message = "services.renews-iroh.bots.<name>.passwordFile must be an absolute path, not a Nix store path";
      }
    ];

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ 119 ];

    systemd.services.renews-iroh = {
      description = "Renews NNTP server (iroh)";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [
        "network-online.target"
        "sops-nix.service"
        "agenix.service"
      ];
      serviceConfig = {
        Type = "simple";
        DynamicUser = true;
        StateDirectory = cfg.stateDirectory;
        StateDirectoryMode = "0700";
        UMask = "0077";
        ExecStartPre = initScript;
        ExecStart = "${cfg.package}/bin/renews --config ${configFile}";
        ExecReload = "${pkgs.coreutils}/bin/kill -HUP $MAINPID";
        Restart = "on-failure";
        RestartSec = "3s";
        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectClock = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictSUIDSGID = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;
        LockPersonality = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        SystemCallArchitectures = "native";
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
      };
    };
  };
}
