import repro_project_dsl
import repro_dsl_stdlib/nixpkgs_pin

# Genuine canonical-Nix development outputs; the executable remains zstd.
# The existing zstd floor/channel is retained separately on every platform.
when defined(linux):
  package evmZstdDevelopment:
    provisioning:
      nixPackage "nixpkgs#zstd^*", executablePath = "bin/zstd",
        nixpkgsRev = CanonicalNixpkgsRev,
        nixpkgsNarHash = CanonicalNixpkgsNarHash
    executable evmZstdDevelopment:
      name: "zstd"
