{
  inputs = {
    nixpkgs.url      = "github:NixOS/nixpkgs/nixos-25.05";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url  = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
      in
      with pkgs;
      {
        devShells.default = mkShell {
          nativeBuildInputs = [
            ffmpeg
            openssl
            opencv
            pkg-config
            rust-bin.stable.latest.default
            rust-analyzer
          ];
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
        };
      }
    );
}
