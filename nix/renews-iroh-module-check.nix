# Evaluation check for nix/renews-iroh.nix. The top-level server and a named
# instance must render separate units, state directories, configs, and
# credentials, and two servers that share a state directory or listen
# address must fail their assertions. A stub package keeps this from
# building renews.
{
  lib,
  pkgs,
  system,
  module,
}:
let
  peer = "0000000000000000000000000000000000000000000000000000000000000001";
  stub = pkgs.writeShellScriptBin "renews" "exit 0";

  evaluate =
    extra:
    (lib.nixosSystem {
      inherit system;
      modules = [
        module
        {
          boot.isContainer = true;
          system.stateVersion = "25.05";
          services.renews-iroh = {
            enable = true;
            package = stub;
            bots.top.passwordFile = "/run/keys/top";
            instances.swarm = {
              listenAddress = "127.0.0.1:1119";
              siteName = "swarm.test";
              secretKeyFile = "/run/keys/swarm-key";
              allowedPeers.member = peer;
              bots.member.passwordFile = "/run/keys/member";
              groups = [ "bots.claims" ];
            };
            instances.disabled.enable = false;
          };
        }
        extra
      ];
    }).config;

  failedAssertions =
    config: map (assertion: assertion.message) (lib.filter (assertion: !assertion.assertion) config.assertions);

  valid = evaluate { };
  clashing = evaluate {
    services.renews-iroh.instances.clash = {
      listenAddress = "127.0.0.1:119";
      stateDirectory = "renews";
    };
  };

  units = valid.systemd.services;
  top = units.renews-iroh.serviceConfig;
  swarm = units.renews-iroh-swarm.serviceConfig;
  configOf = unit: lib.last (lib.splitString " " unit.ExecStart);
in
assert failedAssertions valid == [ ];
assert !(units ? renews-iroh-disabled);
assert top.StateDirectory == "renews";
assert swarm.StateDirectory == "renews-swarm";
assert top.DynamicUser && swarm.DynamicUser;
assert top.LoadCredential == [ "bot-top:/run/keys/top" ];
assert
  swarm.LoadCredential == [
    "bot-member:/run/keys/member"
    "iroh-key:/run/keys/swarm-key"
  ];
assert builtins.elem "AF_NETLINK" swarm.RestrictAddressFamilies;
assert lib.length (failedAssertions clashing) == 2;
pkgs.runCommand "renews-iroh-module" { } ''
  top_config=${configOf top}
  swarm_config=${configOf swarm}
  swarm_init=${swarm.ExecStartPre}

  grep -Fx 'addr = "127.0.0.1:119"' "$top_config"
  grep -Fx 'key_path = "/var/lib/renews/iroh.key"' "$top_config"

  grep -Fx 'addr = "127.0.0.1:1119"' "$swarm_config"
  grep -Fx 'site_name = "swarm.test"' "$swarm_config"
  grep -Fx 'db_path = "sqlite:///var/lib/renews-swarm/news.db"' "$swarm_config"
  grep -Fx 'key_path = "/var/lib/renews-swarm/iroh.key"' "$swarm_config"
  grep -Fx '"${peer}"' "$swarm_config"

  grep -F '"$CREDENTIALS_DIRECTORY/iroh-key" /var/lib/renews-swarm/iroh.key' "$swarm_init"
  grep -F '"$CREDENTIALS_DIRECTORY/bot-member"' "$swarm_init"
  grep -F "add-group bots.claims" "$swarm_init"
  if grep -F '/run/keys/' "$swarm_init"; then
    echo "init script reads a password or key path instead of its credential" >&2
    exit 1
  fi

  touch "$out"
''
