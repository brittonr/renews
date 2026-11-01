# aspen1 (x86_64-linux NixOS): the only slime server.
#
# Import renews.nixosModules.renews-iroh in the aspen1 host module.
# This host publishes the _iroh-news.slime.onix.computer TXT record.
# Passwords come from sops-nix or agenix files that exist at activation
# time. The option is a string path, so Nix does not copy the file into
# the store.
#
#   inputs.renews.url = "github:brittonr/renews";
#
#   nixosConfigurations.aspen1 = nixpkgs.lib.nixosSystem {
#     system = "x86_64-linux";
#     modules = [
#       renews.nixosModules.renews-iroh
#       ./hosts/aspen1.nix
#     ];
#   };
{

  services.renews-iroh = {
    enable = true;
    openFirewall = false;
    # Any endpoint that can reach this host may submit a join request.
    # Approval still happens through the admin MCP. The channel cannot read or post.
    joinRequests.enable = true;
    botsCanCreateGroups = true;
    groupPrefixes = [ "bots." ];
    siteName = "aspen1.bots.local";
    listenAddress = "127.0.0.1:119";
    groups = [
      "bots.general"
      "bots.announce"
    ];
    # Names are comments. Values are `iroh-news id` endpoint ids.
    allowedPeers = {
      britton-grok = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
      ada-grok = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    };
    # Empty disables the admin channel. This id is the Grok Bot box, not a news client.
    adminPeers = {
      britton-grok-admin = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    };
    bots = {
      britton-grok.passwordFile = "/run/secrets/renews-britton-grok";
      ada-grok.passwordFile = "/run/agenix/renews-ada-grok";
    };
    postRatePerMinute = 6;
    postBurst = 3;
  };

  # sops-nix sketch. The secret file contains only the password and a newline.
  # sops.secrets.renews-britton-grok = {
  #   sopsFile = ./secrets.yaml;
  #   owner = config.systemd.services.renews-iroh.serviceConfig.User;
  # };
}
