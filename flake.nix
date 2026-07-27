{
  description = "Nixos TUI Installer";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    fenix.url = "github:nix-community/fenix";
    disko.url = "github:nix-community/disko/latest";
  };

  outputs = inputs@{ nixpkgs, flake-parts, ... }:
  let
    systems = [ "x86_64-linux" "aarch64-linux" ];

    # Build an installer ISO for `system`, with `nixosWizard` preinstalled.
    # `system` is threaded through to isoimage/config.nix, which uses it for
    # nixpkgs.hostPlatform.
    mkInstallerIso = system: nixosWizard: nixpkgs.lib.nixosSystem {
      specialArgs = { inherit inputs system nixosWizard; };
      modules = [ ./isoimage/config.nix ];
    };
  in
  flake-parts.lib.mkFlake { inherit inputs; } ({ withSystem, ... }: {
    inherit systems;

    perSystem = { pkgs, system, config, inputs', ... }: {
      packages = {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "nixos-wizard";
          version = "0.3.2";

          src = inputs.self;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          postInstall = ''
            wrapProgram $out/bin/nixos-wizard \
            --prefix PATH : ${pkgs.lib.makeBinPath [
              inputs'.disko.packages.disko
              pkgs.bat
              pkgs.nixfmt-rfc-style
              pkgs.nixfmt-classic
              pkgs.util-linux
              pkgs.gawk
              pkgs.gnugrep
              pkgs.gnused
              pkgs.ntfs3g
            ]}
          '';
        };

        isoImage =
          (mkInstallerIso system config.packages.default).config.system.build.isoImage;
      };

      devShells.default = pkgs.mkShell {
        packages = [
          (inputs'.fenix.packages.complete.withComponents [
            "cargo"
            "clippy"
            "rustfmt"
            "rustc"
          ])
          pkgs.rust-analyzer
        ];

        shellHook = ''
          export SHELL=${pkgs.zsh}/bin/zsh
          exec ${pkgs.zsh}/bin/zsh
        '';
      };
    };

    flake.nixosConfigurations =
      builtins.listToAttrs (map (system: {
        name = "installerIso-${system}";
        value = withSystem system ({ config, ... }:
          mkInstallerIso system config.packages.default);
      }) systems)
      // {
        # Kept so the attribute path documented in older READMEs keeps working.
        installerIso = withSystem "x86_64-linux" ({ config, ... }:
          mkInstallerIso "x86_64-linux" config.packages.default);
      };
  });
}
