# NixOS module: publish the slime web viewer on a timer.
#
# Off unless services.iroh-news-viewer.enable is set. The timer runs
# deploy/web-viewer/publish.sh, which snapshots slime, renders static HTML,
# and deploys it as a Cloudflare Worker of static assets. Cloudflare Access
# is the login. This module does not put a password in the Nix store.
{ config, lib, pkgs, ... }:
let
  cfg = config.services.iroh-news-viewer;
  stateDir = "/var/lib/${cfg.stateDirectory}";
  publish = ../deploy/web-viewer/publish.sh;
  wranglerToml = ../deploy/web-viewer/wrangler.toml;
in
{
  options.services.iroh-news-viewer = {
    enable = lib.mkEnableOption "timer that publishes the slime web viewer";

    package = lib.mkOption {
      type = lib.types.package;
      description = ''
        iroh-news package. The flake module sets this to
        `self.packages.''${system}.iroh-news`.
      '';
    };

    wrangler = lib.mkOption {
      type = lib.types.package;
      default = pkgs.wrangler;
      defaultText = lib.literalExpression "pkgs.wrangler";
      description = "Wrangler used to deploy the static site.";
    };

    intervalMinutes = lib.mkOption {
      type = lib.types.ints.positive;
      default = 5;
      description = "How often the publish timer runs, in minutes.";
    };

    workerName = lib.mkOption {
      type = lib.types.str;
      default = "slime-viewer";
      description = ''
        Cloudflare Worker name. Letters, digits, `_`, and `-`, starting
        with a letter or digit, at most 63 characters.
      '';
    };

    stateDirectory = lib.mkOption {
      type = lib.types.str;
      default = "iroh-news-viewer";
      description = ''
        systemd StateDirectory. The iroh secret key, snapshot, and rendered
        site live under `/var/lib/<this>`.
      '';
    };

    configFile = lib.mkOption {
      type = lib.types.str;
      default = "";
      example = "/var/lib/iroh-news-viewer/config.toml";
      description = ''
        Absolute path of the iroh-news config (`server`, `username`,
        `password_file`). A string, not a Nix path, so the file is not
        copied into the Nix store. The `iroh-news-viewer` user must be
        able to read it and the password file.
      '';
    };

    environmentFile = lib.mkOption {
      type = lib.types.str;
      default = "";
      example = "/run/secrets/slime-viewer-env";
      description = ''
        Absolute path of a systemd EnvironmentFile. It must set
        `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`. A string, not
        a Nix path. systemd reads it as root, so it can be mode 0600 root.
      '';
    };

    extraSnapshotArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "--full" ];
      description = ''
        Extra arguments appended to `iroh-news snapshot`. `--out` is set
        by the publish script.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = lib.hasPrefix "/" cfg.configFile;
        message = "services.iroh-news-viewer.configFile must be an absolute path, not a Nix store path";
      }
      {
        assertion = lib.hasPrefix "/" cfg.environmentFile;
        message = "services.iroh-news-viewer.environmentFile must be an absolute path, not a Nix store path";
      }
      {
        assertion = builtins.match "[A-Za-z0-9][A-Za-z0-9_-]{0,62}" cfg.workerName != null;
        message = "services.iroh-news-viewer.workerName must match [A-Za-z0-9][A-Za-z0-9_-]{0,62}";
      }
      {
        assertion = builtins.match "[A-Za-z0-9_-]+" cfg.stateDirectory != null;
        message = "services.iroh-news-viewer.stateDirectory must be a single directory name";
      }
    ];

    environment.systemPackages = [ cfg.package ];

    users.users.iroh-news-viewer = {
      isSystemUser = true;
      group = "iroh-news-viewer";
      home = stateDir;
      createHome = false;
    };
    users.groups.iroh-news-viewer = { };

    systemd.services.iroh-news-viewer = {
      description = "Publish the slime web viewer";
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      serviceConfig = {
        Type = "oneshot";
        User = "iroh-news-viewer";
        Group = "iroh-news-viewer";
        WorkingDirectory = stateDir;
        StateDirectory = cfg.stateDirectory;
        CacheDirectory = cfg.stateDirectory;
        UMask = "0077";
        EnvironmentFile = cfg.environmentFile;
        Environment = [
          "WORKER_NAME=${cfg.workerName}"
          "IROH_NEWS=${cfg.package}/bin/iroh-news"
          "WRANGLER=${cfg.wrangler}/bin/wrangler"
          "SNAPSHOT_DIR=${stateDir}/snapshot"
          "SITE_DIR=${stateDir}/site"
          "HOME=${stateDir}"
          "XDG_CACHE_HOME=/var/cache/${cfg.stateDirectory}"
          "XDG_CONFIG_HOME=${stateDir}/config"
          "WRANGLER_TEMPLATE=${wranglerToml}"
          "WRANGLER_SEND_METRICS=false"
        ];
        ExecStart = lib.escapeShellArgs (
          [
            "${pkgs.bash}/bin/bash"
            "${publish}"
            "--config"
            cfg.configFile
            "--state-dir"
            stateDir
          ]
          ++ cfg.extraSnapshotArgs
        );
        TimeoutStartSec = "30min";
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
        RestrictSUIDSGID = true;
        RestrictRealtime = true;
        LockPersonality = true;
        SystemCallArchitectures = "native";
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
      };
    };

    systemd.timers.iroh-news-viewer = {
      description = "Publish the slime web viewer";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = "2min";
        OnUnitActiveSec = "${toString cfg.intervalMinutes}min";
        AccuracySec = "1min";
        Persistent = true;
      };
    };
  };
}
