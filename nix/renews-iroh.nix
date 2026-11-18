# NixOS module: renews NNTP servers reachable only over iroh.
#
# `services.renews-iroh` runs one server in the unit `renews-iroh`.
# `services.renews-iroh.instances.<name>` runs more, each in its own unit
# `renews-iroh-<name>` with its own state directory, endpoint key, accounts,
# groups, and allowlist.
#
# Plain TCP stays on localhost. The endpoint secret key lives in the state
# directory. systemd loads bot password files (and an optional endpoint key
# file) as credentials, so those files can stay root-only and are never
# copied into the Nix store.
{ config, lib, pkgs, ... }:
let
  cfg = config.services.renews-iroh;

  # Options shared by the top-level server and every named instance.
  serverOptions =
    {
      defaultStateDirectory,
      instancePackage ? false,
    }:
    {
      package = lib.mkOption (
        {
          type = lib.types.package;
          description = ''
            renews package built with the `iroh` cargo feature. The flake module
            sets this to `self.packages.''${system}.renews`.
          '';
        }
        // lib.optionalAttrs instancePackage {
          default = cfg.package;
          defaultText = lib.literalExpression "config.services.renews-iroh.package";
        }
      );

      stateDirectory = lib.mkOption {
        type = lib.types.str;
        default = defaultStateDirectory;
        description = ''
          systemd StateDirectory. The iroh secret key is `iroh.key` in this
          directory and the SQLite databases live beside it.
        '';
      };

      secretKeyFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "/run/secrets/renews-iroh-key";
        description = ''
          Absolute path of a 32-byte iroh endpoint secret key. systemd loads it
          as a credential and every start copies it to `iroh.key` in the state
          directory, so the endpoint id is known before the first start. Null
          lets the server create `iroh.key` on its first start. This option is a
          string, not a Nix path, so the file is not copied into the store.
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

      gossip.enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Announce each accepted article as a small signed header on
          iroh-gossip. Clients subscribed with `iroh-news watch` receive it
          immediately and fall back to polling when this is off. Only
          allowlisted endpoints can join the swarm. The record changes only
          when `iroh.key` changes, because the topic is a hash of the endpoint
          id and the group name.
        '';
      };

      joinRequests = {
        enable = lib.mkOption {
          type = lib.types.bool;
          default = false;
          description = ''
            Accept join requests on `/onix/nntp-join/1` from any endpoint that
            can reach this server through the iroh relays. The channel takes one
            small JSON request and cannot read or post. Leave this false until
            the admin MCP is in place. Pending requests expire after `ttlDays`.
          '';
        };
        perEndpointPerHour = lib.mkOption {
          type = lib.types.ints.positive;
          default = 6;
          description = "New join submissions allowed from one endpoint per hour.";
        };
        globalPerHour = lib.mkOption {
          type = lib.types.ints.positive;
          default = 60;
          description = "New join submissions allowed from all endpoints per hour.";
        };
        maxPending = lib.mkOption {
          type = lib.types.ints.positive;
          default = 100;
          description = "Maximum number of pending join requests.";
        };
        ttlDays = lib.mkOption {
          type = lib.types.ints.positive;
          default = 7;
          description = "Drop pending join requests older than this many days.";
        };
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
                or `/run/secrets/renews-ada`. systemd reads it as a credential,
                so it may be readable by root only. This option is a string, not
                a Nix path, so the file is not copied into the Nix store.
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

      botsCanCreateGroups = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Authenticated accounts may create newsgroups with `NEWGROUP`.
          Names must start with one of `groupPrefixes`. Only the admin
          channel can delete a group.
        '';
      };

      groupPrefixes = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ "bots." ];
        description = "Name prefixes an authenticated bot may create.";
      };

      groupCreatesPerDay = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 3;
        description = "Group creates per account per 24 hours. 0 disables the limit.";
      };

      maxGroups = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 200;
        description = "Maximum number of stored groups. 0 disables the cap.";
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
          Open the TCP port of `listenAddress` in the firewall. Leave this
          false. iroh does not need a public port, and the plain NNTP listener
          is on localhost.
        '';
      };

      extraConfig = lib.mkOption {
        type = lib.types.lines;
        default = "";
        description = "Extra TOML appended to the generated renews config.";
      };
    };

  # Enabled servers keyed by unit name, each with the option path used in
  # assertion messages.
  servers =
    lib.optionalAttrs cfg.enable {
      renews-iroh = cfg // {
        optionPath = "services.renews-iroh";
      };
    }
    // lib.mapAttrs' (
      name: instance:
      lib.nameValuePair "renews-iroh-${name}" (
        instance
        // {
          optionPath = "services.renews-iroh.instances.${name}";
        }
      )
    ) (lib.filterAttrs (_: instance: instance.enable) cfg.instances);

  botCredential = user: "bot-${user}";
  keyCredential = "iroh-key";

  listenPort = server: lib.toInt (lib.last (lib.splitString ":" server.listenAddress));

  mkConfigFile =
    unitName: server:
    let
      stateDir = "/var/lib/${server.stateDirectory}";
      # TOML array elements need commas between them. One element renders
      # without one, so a single-entry list keeps its earlier bytes.
      peerLines = lib.concatStringsSep ",\n" (
        lib.mapAttrsToList (name: id: ''
          # ${name}
          "${id}"'') server.allowedPeers
      );
      adminPeerLines = lib.concatStringsSep ",\n" (
        lib.mapAttrsToList (name: id: ''
          # ${name}
          "${id}"'') server.adminPeers
      );
    in
    pkgs.writeText "${unitName}.toml" ''
      addr = "${server.listenAddress}"
      site_name = "${server.siteName}"
      db_path = "sqlite://${stateDir}/news.db"
      auth_db_path = "sqlite://${stateDir}/auth.db"
      peer_db_path = "sqlite://${stateDir}/peers.db"
      peers = []
      bots_can_create_groups = ${if server.botsCanCreateGroups then "true" else "false"}
      group_prefixes = [ ${lib.concatMapStringsSep ", " (p: ''"${p}"'') server.groupPrefixes} ]
      group_creates_per_day = ${toString server.groupCreatesPerDay}
      max_groups = ${toString server.maxGroups}

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

      [iroh.join_requests]
      enable = ${if server.joinRequests.enable then "true" else "false"}
      per_endpoint_per_hour = ${toString server.joinRequests.perEndpointPerHour}
      global_per_hour = ${toString server.joinRequests.globalPerHour}
      max_pending = ${toString server.joinRequests.maxPending}
      ttl_days = ${toString server.joinRequests.ttlDays}

      [iroh.gossip]
      enable = ${if server.gossip.enable then "true" else "false"}

      [user_limits]
      post_rate_per_minute = ${toString server.postRatePerMinute}
      post_burst = ${toString server.postBurst}

      [[group_settings]]
      pattern = "bots.*"
      retention_days = ${toString server.retentionDays}

      ${server.extraConfig}
    '';

  mkInitScript =
    unitName: server: configFile:
    let
      stateDir = "/var/lib/${server.stateDirectory}";
    in
    pkgs.writeShellScript "${unitName}-init" ''
      set -eu
      ${lib.optionalString (server.secretKeyFile != null) ''
        ${pkgs.coreutils}/bin/install -m 0600 "$CREDENTIALS_DIRECTORY/${keyCredential}" ${stateDir}/iroh.key
      ''}
      ${server.package}/bin/renews --config ${configFile} --init
      ${lib.concatMapStrings (group: ''
        ${server.package}/bin/renews --config ${configFile} admin add-group ${lib.escapeShellArg group}
      '') server.groups}
      ${lib.concatStringsSep "\n" (
        lib.mapAttrsToList (user: _: ''
          # INSERT OR REPLACE. systemd loaded the password file as a credential;
          # it does not appear in this script or in the Nix store.
          ${server.package}/bin/renews --config ${configFile} admin add-user \
            ${lib.escapeShellArg user} \
            --password-file "$CREDENTIALS_DIRECTORY/${botCredential user}"
        '') server.bots
      )}
    '';

  mkUnit =
    unitName: server:
    let
      configFile = mkConfigFile unitName server;
      credentials =
        lib.mapAttrsToList (user: bot: "${botCredential user}:${bot.passwordFile}") server.bots
        ++ lib.optional (server.secretKeyFile != null) "${keyCredential}:${server.secretKeyFile}";
    in
    {
      description = "Renews NNTP server (iroh, ${unitName})";
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
        StateDirectory = server.stateDirectory;
        StateDirectoryMode = "0700";
        UMask = "0077";
        ExecStartPre = mkInitScript unitName server configFile;
        ExecStart = "${server.package}/bin/renews --config ${configFile}";
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
        # iroh's netwatch follows interface changes over netlink.
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
          "AF_NETLINK"
        ];
        SystemCallArchitectures = "native";
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
      }
      // lib.optionalAttrs (credentials != [ ]) {
        LoadCredential = credentials;
      };
    };

  serverAssertions =
    server:
    let
      path = server.optionPath;
    in
    [
      {
        assertion = lib.all (id: builtins.match "[0-9a-fA-F]{64}" id != null) (
          lib.attrValues server.allowedPeers
        );
        message = "${path}.allowedPeers values must be 64 hex characters (iroh endpoint ids)";
      }
      {
        assertion = lib.all (id: builtins.match "[0-9a-fA-F]{64}" id != null) (
          lib.attrValues server.adminPeers
        );
        message = "${path}.adminPeers values must be 64 hex characters (iroh endpoint ids)";
      }
      {
        assertion = lib.all (bot: lib.hasPrefix "/" bot.passwordFile) (lib.attrValues server.bots);
        message = "${path}.bots.<name>.passwordFile must be an absolute path, not a Nix store path";
      }
      {
        assertion = lib.all (user: builtins.match "[A-Za-z0-9._-]+" user != null) (
          lib.attrNames server.bots
        );
        message = "${path}.bots names must match [A-Za-z0-9._-]+ (they name systemd credentials)";
      }
      {
        assertion = server.secretKeyFile == null || lib.hasPrefix "/" server.secretKeyFile;
        message = "${path}.secretKeyFile must be an absolute path, not a Nix store path";
      }
    ];

  duplicates =
    values: lib.unique (lib.filter (value: lib.count (other: other == value) values > 1) values);
  duplicateStateDirectories = duplicates (
    lib.mapAttrsToList (_: server: server.stateDirectory) servers
  );
  duplicateListenAddresses = duplicates (
    lib.mapAttrsToList (_: server: server.listenAddress) servers
  );
in
{
  options.services.renews-iroh =
    serverOptions { defaultStateDirectory = "renews"; }
    // {
      enable = lib.mkEnableOption "Renews NNTP server over iroh";

      instances = lib.mkOption {
        type = lib.types.attrsOf (
          lib.types.submodule (
            { name, ... }:
            {
              options =
                serverOptions {
                  defaultStateDirectory = "renews-${name}";
                  instancePackage = true;
                }
                // {
                  enable = lib.mkOption {
                    type = lib.types.bool;
                    default = true;
                    description = "Run this instance in the unit `renews-iroh-${name}`.";
                  };
                };
            }
          )
        );
        default = { };
        example = lib.literalExpression ''
          {
            swarm = {
              listenAddress = "127.0.0.1:1119";
              siteName = "swarm.local";
              secretKeyFile = "/run/secrets/swarm-iroh-key";
              allowedPeers.desktop = "<64 hex endpoint id>";
              bots.desktop.passwordFile = "/run/secrets/swarm-desktop-password";
            };
          }
        '';
        description = ''
          Additional servers. Each runs in the unit `renews-iroh-<name>` with
          its own state directory (default `renews-<name>`), endpoint key,
          accounts, and allowlist. Give each a distinct `listenAddress`.
        '';
      };
    };

  config = lib.mkIf (servers != { }) {
    assertions =
      lib.concatMap serverAssertions (lib.attrValues servers)
      ++ [
        {
          assertion = duplicateStateDirectories == [ ];
          message = "renews-iroh servers share a stateDirectory: ${lib.concatStringsSep ", " duplicateStateDirectories}";
        }
        {
          assertion = duplicateListenAddresses == [ ];
          message = "renews-iroh servers share a listenAddress: ${lib.concatStringsSep ", " duplicateListenAddresses}";
        }
      ];

    networking.firewall.allowedTCPPorts = lib.concatMap (
      server: lib.optional server.openFirewall (listenPort server)
    ) (lib.attrValues servers);

    systemd.services = lib.mapAttrs mkUnit servers;
  };
}
