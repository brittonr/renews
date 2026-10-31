{
  description = "Renews NNTP server and the iroh-news bot client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ rust-overlay.overlays.default ];
        pkgs = import nixpkgs { inherit system overlays; };
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "clippy"
            "rustfmt"
          ];
          targets = [
            "x86_64-unknown-linux-musl"
            "aarch64-unknown-linux-musl"
          ];
        };
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
        # cleanCargoSource keeps only Rust and TOML. sqlx::migrate! embeds the
        # .sql files, and the server tests embed the admin PGP keys.
        src = pkgs.lib.cleanSourceWith {
          src = pkgs.lib.cleanSource ./.;
          filter =
            path: type:
            (craneLib.filterCargoSources path type)
            || pkgs.lib.hasSuffix ".sql" (baseNameOf path)
            || pkgs.lib.hasSuffix ".asc" (baseNameOf path);
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.sqlite ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        renews = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            pname = "renews";
            cargoExtraArgs = "-p renews --no-default-features --features iroh";
            meta.mainProgram = "renews";
          }
        );

        iroh-news = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            pname = "iroh-news";
            cargoExtraArgs = "-p iroh-news";
            meta.mainProgram = "iroh-news";
          }
        );

        # Static musl build of the bot CLI for the current Linux architecture.
        # iroh-news does not link SQLite. The server package stays dynamically linked.
        muslTarget =
          if pkgs.stdenv.hostPlatform.isAarch64 then
            "aarch64-unknown-linux-musl"
          else if pkgs.stdenv.hostPlatform.isx86_64 then
            "x86_64-unknown-linux-musl"
          else
            null;

        iroh-news-static =
          if muslTarget == null then
            null
          else
            craneLib.buildPackage {
              pname = "iroh-news-static";
              inherit src;
              strictDeps = true;
              CARGO_BUILD_TARGET = muslTarget;
              cargoExtraArgs = "-p iroh-news";
              doCheck = false;
              nativeBuildInputs = [
                pkgs.pkg-config
                pkgs.pkgsStatic.stdenv.cc
              ];
              meta.mainProgram = "iroh-news";
            };
      in
      {
        packages = {
          inherit renews iroh-news;
          default = renews;
        }
        // pkgs.lib.optionalAttrs (iroh-news-static != null) {
          inherit iroh-news-static;
        };

        devShells.default = craneLib.devShell {
          inputsFrom = [ renews ];
          packages = [
            rustToolchain
            pkgs.sqlite
            pkgs.pkg-config
            pkgs.postgresql
          ];
        };
      }
    )
    // {
      nixosModules.renews-iroh =
        {
          pkgs,
          lib,
          ...
        }:
        {
          imports = [ ./nix/renews-iroh.nix ];
          services.renews-iroh.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.renews;
        };
    };
}
