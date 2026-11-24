# Evaluation check for nix/renews-iroh.nix. The top-level server and a named
# instance must render separate units, state directories, configs, and
# credentials, and two servers that share a state directory or listen
# address must fail their assertions. Both configs must parse as TOML,
# including arrays with more than one entry. A stub package keeps this from
# building renews.
{
  lib,
  pkgs,
  system,
  module,
}:
let
  peer = "0000000000000000000000000000000000000000000000000000000000000001";
  otherPeer = "0000000000000000000000000000000000000000000000000000000000000002";
  stub = pkgs.writeShellScriptBin "renews" "exit 0";
  stubBridge = pkgs.writeShellScriptBin "dumbpipe" "exit 0";

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
            relayUrls = [ "https://relay.example.com" ];
            bots.top.passwordFile = "/run/keys/top";
            instances.swarm = {
              listenAddress = "127.0.0.1:1119";
              siteName = "swarm.test";
              secretKeyFile = "/run/keys/swarm-key";
              irohBindAddress = "100.64.0.1:1119";
              relays = false;
              allowedPeers.member = peer;
              allowedPeers.other = otherPeer;
              adminPeers.member = peer;
              adminPeers.other = otherPeer;
              groupPrefixes = [
                "bots."
                "swarm."
              ];
              bots.member.passwordFile = "/run/keys/member";
              groups = [ "bots.claims" ];
              groupDescriptions."bots.claims" = "Active scope claims";
              legacyBridge = {
                enable = true;
                package = stubBridge;
              };
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
  zeroPort = evaluate {
    services.renews-iroh.instances.swarm.irohBindAddress = lib.mkForce "100.64.0.1:0";
  };
  relayClash = evaluate {
    services.renews-iroh.relayUrls = [ "https://relay.example.com" ];
    services.renews-iroh.relays = false;
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
assert lib.length (failedAssertions zeroPort) == 1;
assert lib.length (failedAssertions relayClash) == 1;

# The bridge unit exists only where it is enabled, forwards to that server's
# plain listener, and writes its ticket into its own state directory.
assert !(units ? renews-iroh-dumbpipe);
assert lib.hasInfix "listen-tcp --host 127.0.0.1:1119" units.renews-iroh-swarm-dumbpipe.serviceConfig.ExecStart;
assert units.renews-iroh-swarm-dumbpipe.serviceConfig.StateDirectory == "renews-swarm-dumbpipe";
assert units.renews-iroh-swarm-dumbpipe.serviceConfig.StandardOutput == "append:/var/lib/renews-swarm-dumbpipe/ticket";
pkgs.runCommand "renews-iroh-module" { nativeBuildInputs = [ pkgs.python3 ]; } ''
  top_config=${configOf top}
  swarm_config=${configOf swarm}
  swarm_init=${swarm.ExecStartPre}

  grep -Fx 'addr = "127.0.0.1:119"' "$top_config"
  grep -Fx 'key_path = "/var/lib/renews/iroh.key"' "$top_config"

  grep -Fx 'addr = "127.0.0.1:1119"' "$swarm_config"
  grep -Fx 'site_name = "swarm.test"' "$swarm_config"
  grep -Fx 'db_path = "sqlite:///var/lib/renews-swarm/news.db"' "$swarm_config"
  grep -Fx 'key_path = "/var/lib/renews-swarm/iroh.key"' "$swarm_config"

  python3 - "$top_config" "$swarm_config" <<'PY'
  import sys, tomllib
  top, swarm = (tomllib.load(open(path, "rb")) for path in sys.argv[1:])
  assert top["iroh"]["allowed_peers"] == [], top["iroh"]
  # Defaults render nothing, so a server that sets neither keeps its bytes.
  assert {"bind_addr", "relays", "wait_for_relay"}.isdisjoint(top["iroh"]), top["iroh"]
  peers = ["${peer}", "${otherPeer}"]
  assert swarm["iroh"]["allowed_peers"] == peers, swarm["iroh"]
  assert swarm["iroh"]["admin_peers"] == peers, swarm["iroh"]
  assert swarm["iroh"]["bind_addr"] == "100.64.0.1:1119", swarm["iroh"]
  assert swarm["iroh"]["relays"] is False, swarm["iroh"]
  assert top["iroh"]["relay_urls"] == ["https://relay.example.com"], top["iroh"]
  assert swarm["iroh"]["wait_for_relay"] is False, swarm["iroh"]
  assert swarm["group_prefixes"] == ["bots.", "swarm."], swarm["group_prefixes"]
  PY

  grep -F '"$CREDENTIALS_DIRECTORY/iroh-key" /var/lib/renews-swarm/iroh.key' "$swarm_init"
  grep -F '"$CREDENTIALS_DIRECTORY/bot-member"' "$swarm_init"
  grep -F "add-group bots.claims --description 'Active scope claims'" "$swarm_init"
  if grep -F '/run/keys/' "$swarm_init"; then
    echo "init script reads a password or key path instead of its credential" >&2
    exit 1
  fi

  touch "$out"
''
