{
  description = "tma: agent state monitor, picker, jump, and stamping for tmux";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      overlays.default = final: _prev: {
        tma = final.callPackage ./nix/package.nix { };
      };

      packages = forAllSystems (pkgs: rec {
        tma = pkgs.callPackage ./nix/package.nix { };
        default = tma;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.tma ];
          packages = with pkgs; [
            clippy
            rustfmt
            rust-analyzer
            tmux
            mise
            mdbook
            # Drafts the CHANGELOG entry from the commit log (`mise run changelog`).
            git-cliff
          ];
        };
      });

      checks = forAllSystems (pkgs: {
        tma = self.packages.${pkgs.stdenv.hostPlatform.system}.tma;
      });

      homeModules = rec {
        tma = import ./nix/hm-module.nix;
        default = tma;
      };
      # Older home-manager setups look for this attribute name.
      homeManagerModules = self.homeModules;

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
