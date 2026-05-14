{
  description = "CodeTracer EVM Recorder development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            # Solidity/EVM tools
            # Expected versions: solc 0.8.28+, foundry 1.1.0+ (forge, cast, anvil)
            solc
            foundry

            # Rust build dependencies
            rustc
            cargo
            capnproto
            pkg-config
            openssl

            # Required by libcodetracer_trace_writer (Nim FFI static lib).
            # Without it, link fails with `ld: cannot find -lzstd`.
            zstd
          ];

          shellHook = ''
            echo "CodeTracer EVM Recorder dev shell"
            echo "solc: $(solc --version | tail -1)"
            echo "forge: $(forge --version)"
            echo "anvil: $(anvil --version)"
          '';
        };
      }
    );
}
