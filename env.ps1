# codetracer-evm-recorder Windows dev environment (PowerShell)
# Usage: . .\env.ps1
#
# The recorder builds and tests with a plain `cargo build` / `cargo test`.
# Its Windows requirements are:
#
#   1. The shared CodeTracer toolchain (Rust, Nim + nimble, just, Cap'n Proto,
#      MSVC).  These are provisioned by the main `codetracer` repo's env.ps1,
#      which this script dot-sources.  Nim is needed because the
#      `codetracer_trace_writer_nim` crate's build script compiles a Nim
#      static library.
#
#   2. An explicit MSVC linker for the `x86_64-pc-windows-msvc` target -- see
#      the comment block below `WINDOWS_DIY_CL_EXE`.
#
#   3. The Solidity compiler (`solc`) and Foundry's `anvil`.  The end-to-end
#      tests compile the `contracts/*.sol` fixtures with `solc` and replay
#      transactions against a local `anvil` node; the Nix dev shell pins
#      `solc 0.8.28+` and `foundry 1.1.0+`.  Both ship official prebuilt
#      Windows binaries (`solc-windows.exe` from `ethereum/solidity`; the
#      `foundry_stable_win32_amd64.zip` bundle from `foundry-rs/foundry`).

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition

# --- 1. Shared CodeTracer toolchain -----------------------------------------
$env:WINDOWS_DIY_SKIP_FPC = "1"
$env:WINDOWS_DIY_SKIP_LLVM = "1"
$env:WINDOWS_DIY_SKIP_NARGO = "1"
$env:WINDOWS_DIY_SKIP_DOTNET = "1"

$codetracerEnv = Join-Path (Split-Path -Parent $scriptDir) "codetracer\env.ps1"
if (-not (Test-Path $codetracerEnv)) {
    throw "Could not find the shared CodeTracer env.ps1 at $codetracerEnv -- the ``codetracer`` repo must be checked out as a sibling of this repo."
}
. $codetracerEnv

# --- 2. Explicit MSVC linker (immune to Git Bash PATH reordering) -----------
# The `just test` recipe runs `verify-cli-convention-no-silent-skip.sh` via
# bash, which invokes `cargo build`.  A bash login shell re-orders PATH so
# Git Bash's coreutils `link.exe` precedes the MSVC toolchain; pinning the
# linker by absolute path bypasses PATH resolution.
if ($env:WINDOWS_DIY_CL_EXE -and (Test-Path $env:WINDOWS_DIY_CL_EXE)) {
    $msvcBin = Split-Path -Parent $env:WINDOWS_DIY_CL_EXE
    $msvcLink = Join-Path $msvcBin "link.exe"
    if (Test-Path $msvcLink) {
        $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $msvcLink
    }
    if ($env:Path -notlike "$msvcBin;*") {
        $env:Path = "$msvcBin;$($env:Path)"
    }
}

# --- 3. solc + Foundry (anvil) ----------------------------------------------
$devDepsRoot = if ($env:WINDOWS_DIY_INSTALL_ROOT) { $env:WINDOWS_DIY_INSTALL_ROOT }
               elseif (Test-Path "D:\") { "D:\metacraft-dev-deps" }
               else { Join-Path $env:LOCALAPPDATA "codetracer\windows-diy" }

# solc -- the Nix dev shell pins 0.8.28; the `contracts/*.sol` fixtures only
# require `pragma solidity ^0.8.0`.
$solcVersion = "0.8.28"
$solcDir = Join-Path $devDepsRoot "solc\$solcVersion"
$solcExe = Join-Path $solcDir "solc.exe"
if (Test-Path $solcExe) {
    Write-Host "solc $solcVersion already installed"
} else {
    Write-Host "Installing solc $solcVersion..."
    New-Item -ItemType Directory -Force -Path $solcDir | Out-Null
    Invoke-WebRequest -Uri "https://github.com/ethereum/solidity/releases/download/v$solcVersion/solc-windows.exe" -OutFile $solcExe
    if ((& $solcExe --version 2>&1) -notmatch [regex]::Escape($solcVersion)) {
        throw "solc $solcVersion self-check failed"
    }
    Write-Host "Installed solc $solcVersion to $solcDir"
}
if ($env:Path -notlike "*$solcDir*") {
    $env:Path = "$solcDir;$($env:Path)"
}

# Foundry -- the `stable` channel (anvil/cast/forge/chisel).  The dev shell
# pins foundry 1.1.0+; the prebuilt `stable` zip currently ships 1.5.x.
$foundryDir = Join-Path $devDepsRoot "foundry\stable"
$anvilExe = Join-Path $foundryDir "anvil.exe"
if (Test-Path $anvilExe) {
    Write-Host "Foundry (anvil) already installed"
} else {
    Write-Host "Installing Foundry (stable)..."
    New-Item -ItemType Directory -Force -Path $foundryDir | Out-Null
    $foundryZip = Join-Path $env:TEMP "foundry-stable.zip"
    Invoke-WebRequest -Uri "https://github.com/foundry-rs/foundry/releases/download/stable/foundry_stable_win32_amd64.zip" -OutFile $foundryZip
    Expand-Archive -Path $foundryZip -DestinationPath $foundryDir -Force
    Remove-Item $foundryZip -Force -ErrorAction SilentlyContinue
    if (-not (Test-Path $anvilExe)) { throw "anvil.exe missing after Foundry extraction" }
    Write-Host "Installed Foundry to $foundryDir"
}
if ($env:Path -notlike "*$foundryDir*") {
    $env:Path = "$foundryDir;$($env:Path)"
}

Write-Host "solc: $(((& solc --version) 2>&1) | Select-Object -Last 1)"
Write-Host "anvil: $(((& anvil --version) 2>&1) | Select-Object -First 1)"
Write-Host "codetracer-evm-recorder dev environment ready."
