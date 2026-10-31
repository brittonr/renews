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

        # Static musl iroh-news. The host target uses pkgsStatic. The other
        # Linux musl target is cross-compiled so an x86_64 builder can produce
        # the aarch64 bot binary and a DGX Spark can produce the x86_64 one.
        mkIrohNewsStatic =
          target: cc:
          craneLib.buildPackage {
            pname = "iroh-news-static";
            inherit src;
            strictDeps = true;
            doCheck = false;
            CARGO_BUILD_TARGET = target;
            cargoExtraArgs = "-p iroh-news";
            nativeBuildInputs = [ cc ];
            env = {
              CC = "${cc}/bin/${cc.targetPrefix}cc";
              "CARGO_TARGET_${
                pkgs.lib.toUpper (pkgs.lib.replaceStrings [ "-" ] [ "_" ] target)
              }_LINKER" =
                "${cc}/bin/${cc.targetPrefix}cc";
            };
            meta.mainProgram = "iroh-news";
          };

        linuxMusl =
          if pkgs.stdenv.hostPlatform.isx86_64 then
            {
              x86_64 = {
                target = "x86_64-unknown-linux-musl";
                cc = pkgs.pkgsStatic.stdenv.cc;
              };
              aarch64 = {
                target = "aarch64-unknown-linux-musl";
                cc = pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc;
              };
            }
          else if pkgs.stdenv.hostPlatform.isAarch64 then
            {
              x86_64 = {
                target = "x86_64-unknown-linux-musl";
                cc = pkgs.pkgsCross.musl64.stdenv.cc;
              };
              aarch64 = {
                target = "aarch64-unknown-linux-musl";
                cc = pkgs.pkgsStatic.stdenv.cc;
              };
            }
          else
            null;

        iroh-news-static-x86_64 =
          if linuxMusl == null then null else mkIrohNewsStatic linuxMusl.x86_64.target linuxMusl.x86_64.cc;
        iroh-news-static-aarch64 =
          if linuxMusl == null then
            null
          else
            mkIrohNewsStatic linuxMusl.aarch64.target linuxMusl.aarch64.cc;
        iroh-news-static =
          if pkgs.stdenv.hostPlatform.isAarch64 then
            iroh-news-static-aarch64
          else
            iroh-news-static-x86_64;
      in
      {
        checks = {
          inherit renews;
        };

        packages = {
          inherit renews iroh-news;
          default = renews;
        }
        // pkgs.lib.optionalAttrs (linuxMusl != null) {
          inherit
            iroh-news-static
            iroh-news-static-x86_64
            iroh-news-static-aarch64
            ;
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
