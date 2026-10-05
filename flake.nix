{
  description = "stillo - AI-native terminal browser";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    # overlay は system に依存しないので eachDefaultSystem の外に置く。
    # 中に置くと overlays.<system>.default になって `overlays.default` で参照できない。
    {
      # 自分の nixpkgs に生やす場合:
      #   nixpkgs.overlays = [ inputs.stillo.overlays.default ];
      #   home.packages = [ pkgs.stillo ];
      overlays.default = final: prev: {
        stillo = final.callPackage ./nix/package.nix { };
      };
    }
    // flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        stillo = pkgs.callPackage ./nix/package.nix { };
      in
      {
        # インストール用: nix build .# / nix profile install .# / nix run .
        packages = {
          inherit stillo;
          default = stillo;
        };

        apps.default = {
          type = "app";
          program = "${stillo}/bin/stillo";
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustc
            cargo
            rust-analyzer
            clippy
            rustfmt
            pkg-config
            openssl
          ];

          shellHook = ''
            echo "Rust: $(rustc --version)"
            echo "Cargo: $(cargo --version)"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
