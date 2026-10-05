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
            rustfmt
            clippy
            capnproto
            pkg-config
            openssl

            # Required by libcodetracer_trace_writer (Nim FFI static lib).
            # Without it, link fails with `ld: cannot find -lzstd`.
            zstd

            # Nim toolchain for codetracer_trace_writer_nim's
            # build.rs (compiles the Nim FFI sources to a static
            # library at cargo build time).
            nim
            nimble
            # `git` from nixpkgs, ahead of the host's. On macOS the host's
            # `/usr/bin/git` is an xcode-select trampoline that runs
            # `$DEVELOPER_DIR/usr/bin/xcrun`; in this shell DEVELOPER_DIR is the
            # nixpkgs apple-sdk, whose xcrun (xcbuild) prints "warning: unhandled
            # Platform key FamilyDisplayName" on every call. nimble reads git's
            # stderr together with its stdout, so with that git `nimble install` would
            # reject `git rev-parse HEAD` as "not a valid sha1 hash value".
            git

            # Just for the `just lint` / `just test` entry points.
            just

            # Native portable hook SDK; rules remain tracked in the owning repo.
            prek
            uv
            python3
            editorconfig-checker
            nixfmt
            nodePackages.prettier
            opentofu
          ];

          # `cargo <subcommand>` looks for `cargo-<subcommand>` in
          # `$CARGO_HOME/bin` BEFORE it searches PATH. On any machine with
          # rustup — including the self-hosted macOS runner — that directory
          # holds rustup's proxies, so `cargo fmt` and `cargo clippy` run
          # rustup's `cargo-fmt` / `cargo-clippy` instead of the rustfmt and
          # clippy above, and fail with "'cargo-fmt' is not installed for the
          # toolchain".
          #
          # The shell therefore gets its own CARGO_HOME with an empty `bin/`,
          # so subcommand lookup falls through to PATH. `registry/` and `git/`
          # are symlinks to the real CARGO_HOME, and so are its config and
          # credentials when present: the download cache is shared, and only
          # the proxy directory is left behind.
          shellHook = ''
            _ct_hook_root="$(${pkgs.git}/bin/git rev-parse --show-toplevel 2>/dev/null || true)"
            if [ -n "$_ct_hook_root" ] && [ "$PWD" = "$_ct_hook_root" ] \
              && [ -f "$_ct_hook_root/flake.nix" ] \
              && [ "$(${pkgs.coreutils}/bin/sha256sum "$_ct_hook_root/flake.nix" | ${pkgs.coreutils}/bin/cut -d' ' -f1)" = "${builtins.hashFile "sha256" ./flake.nix}" ]; then
              _ct_matching_repro="''${REPROBUILD_REPRO:-$(command -v repro)}"
              ${pkgs.python3}/bin/python3 tools/install-canonical-hooks.py --repro "$_ct_matching_repro" --bootstrap-managed || return $?
              ${pkgs.python3}/bin/python3 tools/install-canonical-hooks.py --repro "$_ct_matching_repro" || return $?
              unset _ct_matching_repro
            fi
            unset _ct_hook_root
            _ct_real_cargo_home="''${CARGO_HOME:-$HOME/.cargo}"
            _ct_cargo_home="''${XDG_CACHE_HOME:-$HOME/.cache}/codetracer-evm-recorder/cargo-home"
            if [ "$_ct_real_cargo_home" != "$_ct_cargo_home" ]; then
              mkdir -p "$_ct_cargo_home" \
                "$_ct_real_cargo_home/registry" "$_ct_real_cargo_home/git"
              # Re-pointed on every entry, so a changed CARGO_HOME is followed
              # rather than left sharing the previous one's cache. Only a link
              # is ever replaced; a real file placed here is left alone.
              for _ct_entry in registry git config.toml credentials.toml; do
                if [ -e "$_ct_real_cargo_home/$_ct_entry" ] &&
                  { [ -L "$_ct_cargo_home/$_ct_entry" ] ||
                    [ ! -e "$_ct_cargo_home/$_ct_entry" ]; }; then
                  ln -sfn "$_ct_real_cargo_home/$_ct_entry" "$_ct_cargo_home/$_ct_entry"
                fi
              done
              export CARGO_HOME="$_ct_cargo_home"
            fi
            unset _ct_real_cargo_home _ct_cargo_home _ct_entry
            echo "CodeTracer EVM Recorder dev shell"
            echo "solc: $(solc --version | tail -1)"
            echo "forge: $(forge --version)"
            echo "anvil: $(anvil --version)"
          '';
        };
      }
    );
}
