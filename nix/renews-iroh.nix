# NixOS module: renews NNTP server reachable only over iroh.
#
# Plain TCP stays on localhost. The endpoint secret key lives in the systemd
# state directory. Peers are not configured, so groups such as bots.* are not
# offered to other news servers.
{ config, lib, pkgs, ... }:
let
  cfg = config.services.renews-iroh;
  stateDir = "/var/lib/${cfg.stateDirectory}";
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
      ${lib.concatMapStrings (id: ''"${id}"'' + "\n") cfg.allowedPeers}
    ]

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
  '';
in
{
  options.services.renews-iroh = {
    enable = lib.mkEnableOption "Renews NNTP server over iroh";

    package = lib.mkOption {
      type = lib.types.package;
      description = ''
        renews package built with the `iroh` cargo feature and without a
        requirement on PostgreSQL. The flake's NixOS module sets this.
      '';
    };

    stateDirectory = lib.mkOption {
      type = lib.types.str;
      default = "renews";
      description = "systemd StateDirectory. The iroh secret key is stored here.";
    };

    listenAddress = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:119";
      description = "Plain NNTP listen address. iroh deployments keep this on localhost.";
    };

    siteName = lib.mkOption {
      type = lib.types.str;
      default = "bots.local";
      description = "Hostname used in generated Message-IDs.";
    };

    allowedPeers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        Endpoint IDs (hex) allowed to open an iroh connection.
        An empty list rejects every peer. Obtain an id with `iroh-news id`.
      '';
    };

    groups = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "bots.general" ];
      description = ''
        Newsgroups created on startup. No NNTP peers are configured, so these
        groups are not offered upstream.
      '';
    };

    retentionDays = lib.mkOption {
      type = lib.types.ints.positive;
      default = 30;
      description = "Retention for the bots.* hierarchy.";
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
        assertion = lib.all (id: builtins.match "[0-9a-fA-F]+" id != null) cfg.allowedPeers;
        message = "services.renews-iroh.allowedPeers entries must be hex endpoint ids";
      }
    ];

    systemd.services.renews-iroh = {
      description = "Renews NNTP server (iroh)";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      serviceConfig = {
        Type = "simple";
        DynamicUser = true;
        StateDirectory = cfg.stateDirectory;
        ExecStartPre = initScript;
        ExecStart = "${cfg.package}/bin/renews --config ${configFile}";
        ExecReload = "${pkgs.coreutils}/bin/kill -HUP $MAINPID";
        Restart = "on-failure";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        LockPersonality = true;
      };
    };
  };
}
