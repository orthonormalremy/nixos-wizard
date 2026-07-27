{ config, pkgs, modulesPath, nixosWizard, system, ... }: {
  imports = [
    "${modulesPath}/installer/cd-dvd/installation-cd-minimal.nix"
  ];

  environment.etc."issue".text = ''
                                    \e[92m<<< NixOS ${config.system.nixos.label} \r (\m) - \l >>>\e[0m

             \e[38;5;27m▓▓▓▓       \e[38;5;81m▒▒▒▒    ▒▒▒▒
              \e[38;5;27m▓▓▓▓       \e[38;5;81m▒▒▒▒  ▒▒▒▒            \e[38;5;27m                       ▓▓▓
               \e[38;5;27m▓▓▓▓       \e[38;5;81m▒▒▒▒▒▒▒▒             \e[38;5;27m  ▓▓▓          ▓▓     ▓▓▓▓▓                  \e[38;5;81m    ▒▒▒▒▒▒▒▒▒        ▒▒▒▒▒▒▒▒▒
          \e[38;5;27m▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓\e[38;5;81m▒▒▒▒▒▒    \e[38;5;27m▓▓        \e[38;5;27m ▓▓▓▓▓        ▓▓▓▓     ▓▓▓                   \e[38;5;81m   ▒▒▒▒▒▒▒▒▒▒▒      ▒▒▒▒▒▒▒▒▒▒▒
         \e[38;5;27m▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓\e[38;5;81m▒▒▒▒    \e[38;5;27m▓▓▓        \e[38;5;27m ▓▓▓▓▓▓       ▓▓▓▓                           \e[38;5;81m  ▒▒▒▒▒   ▒▒▒▒▒    ▒▒▒▒    ▒▒▒▒▒
                \e[38;5;81m▒▒▒▒         \e[38;5;81m▒▒▒▒  \e[38;5;27m▓▓▓▓        \e[38;5;27m ▓▓▓▓▓▓▓      ▓▓▓▓                           \e[38;5;81m  ▒▒▒▒     ▒▒▒▒   ▒▒▒▒
               \e[38;5;81m▒▒▒▒           \e[38;5;81m▒▒▒▒\e[38;5;27m▓▓▓▓         \e[38;5;27m ▓▓▓▓▓▓▓▓     ▓▓▓▓    ▓▓▓▓     ▓▓▓      ▓▓▓  \e[38;5;81m  ▒▒▒       ▒▒▒   ▒▒▒▒
       \e[38;5;81m▒▒▒▒▒▒▒▒▒▒▒             \e[38;5;81m▒▒\e[38;5;27m▓▓▓▓          \e[38;5;27m ▓▓▓▓ ▓▓▓▓    ▓▓▓▓    ▓▓▓▓     ▓▓▓▓    ▓▓▓▓  \e[38;5;81m  ▒▒▒       ▒▒▒   ▒▒▒▒▒
       \e[38;5;81m ▒▒▒▒▒▒▒▒▒\e[38;5;27m               \e[38;5;27m▓▓▓▓▓▓▓▓▓      \e[38;5;27m ▓▓▓▓  ▓▓▓▓   ▓▓▓▓     ▓▓▓      ▓▓▓▓  ▓▓▓▓   \e[38;5;81m  ▒▒▒       ▒▒▒    ▒▒▒▒▒▒▒▒▒
            \e[38;5;81m▒▒▒▒\e[38;5;27m▓▓             \e[38;5;27m▓▓▓▓▓▓▓▓▓▓▓     \e[38;5;27m ▓▓▓▓   ▓▓▓▓  ▓▓▓▓     ▓▓▓       ▓▓▓▓▓▓▓▓    \e[38;5;81m  ▒▒▒       ▒▒▒      ▒▒▒▒▒▒▒▒▒
           \e[38;5;81m▒▒▒▒\e[38;5;27m▓▓▓▓           \e[38;5;27m▓▓▓▓             \e[38;5;27m ▓▓▓▓    ▓▓▓▓ ▓▓▓▓     ▓▓▓        ▓▓▓▓▓▓     \e[38;5;81m  ▒▒▒       ▒▒▒            ▒▒▒▒▒
          \e[38;5;81m▒▒▒▒  \e[38;5;27m▓▓▓▓         \e[38;5;27m▓▓▓▓              \e[38;5;27m ▓▓▓▓     ▓▓▓▓▓▓▓▓     ▓▓▓        ▓▓▓▓▓▓     \e[38;5;81m  ▒▒▒       ▒▒▒             ▒▒▒▒
          \e[38;5;81m▒▒▒    \e[38;5;27m▓▓▓▓\e[38;5;81m▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒        \e[38;5;27m ▓▓▓▓      ▓▓▓▓▓▓▓     ▓▓▓       ▓▓▓▓▓▓▓▓    \e[38;5;81m  ▒▒▒▒     ▒▒▒▒             ▒▒▒▒
          \e[38;5;81m▒▒    \e[38;5;27m▓▓▓▓▓▓\e[38;5;81m▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒         \e[38;5;27m ▓▓▓▓       ▓▓▓▓▓▓    ▓▓▓▓▓     ▓▓▓▓  ▓▓▓▓   \e[38;5;81m  ▒▒▒▒▒   ▒▒▒▒▒   ▒▒▒▒▒    ▒▒▒▒▒
               \e[38;5;27m▓▓▓▓▓▓▓▓       \e[38;5;81m▒▒▒▒             \e[38;5;27m ▓▓▓▓        ▓▓▓▓▓    ▓▓▓▓▓    ▓▓▓▓    ▓▓▓▓  \e[38;5;81m   ▒▒▒▒▒▒▒▒▒▒▒     ▒▒▒▒▒▒▒▒▒▒▒▒
              \e[38;5;27m▓▓▓▓  ▓▓▓▓       \e[38;5;81m▒▒▒▒            \e[38;5;27m  ▓▓          ▓▓▓      ▓▓▓     ▓▓▓      ▓▓▓  \e[38;5;81m    ▒▒▒▒▒▒▒▒▒       ▒▒▒▒▒▒▒▒▒
             \e[38;5;27m▓▓▓▓    ▓▓▓▓       \e[38;5;81m▒▒▒▒\e[0m

The "nixos" and "root" accounts both have \e[33mempty passwords\e[0m. Login using `\e[1;35mssh\e[0m` requires a password to be set using the `\e[1;35mpasswd\e[0m` command.

To set up a wireless connection, run `\e[1;35mnmtui\e[0m`.

Run `\e[1;35msudo nixos-wizard\e[0m` to enter the installer.
Run `\e[1;35mnixos-help\e[0m` for the NixOS manual.

  '';

  environment.systemPackages = [
    pkgs.nixfmt
    pkgs.nixfmt-classic
    nixosWizard
  ];

  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  nixpkgs.hostPlatform = system;
  networking.networkmanager.enable = true;
}
