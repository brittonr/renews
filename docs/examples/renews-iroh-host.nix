# Example NixOS host for a DGX Spark (aarch64) that only exposes NNTP over iroh.
#
# Drop this into a flake that follows brittonr/renews. Passwords come from
# sops-nix or agenix files that exist at activation time. They are absolute
# paths, not Nix paths, so they are not copied into the Nix store.
#
#   inputs.renews.url = "github:brittonr/renews";
#
#   nixosConfigurations.spark = nixpkgs.lib.nixosSystem {
#     system = "aarch64-linux";
#     modules = [
#       renews.nixosModules.renews-iroh
#       ./spark.nix
#     ];
#   };
{

  services.renews-iroh = {
    enable = true;
    openFirewall = false;
    siteName = "spark.bots.local";
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
