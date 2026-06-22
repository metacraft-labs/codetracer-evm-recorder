## Reprobuild dev env + build recipe for codetracer-evm-recorder.
##
## Mirrors the dev shell declared in ``flake.nix`` (Linux/macOS) and
## the Windows DIY env declared in ``env.ps1``. ``repro build`` /
## ``repro test`` reproduce the same artefacts and the same test set
## that ``just build`` / ``just test`` produce today.
##
## Per ``codetracer-specs/Repo-Requirements.md`` §2.8 the recipe
## expresses build and test execution NATIVELY through typed-tool
## edges (`cargo.build`, `cargo.test`). It does NOT delegate to
## `shell(command = "bash scripts/...")` wrappers — delegation
## defeats the engine's incremental-build, action-cache, per-test
## invalidation, and the CI sharding the engine grows into per
## ``reprobuild-specs/CI-Sharding.md``.
##
## On Windows the recipe drives real reprobuild tool provisioning via
## the tarball entries the ``uses:`` packages declare (cargo, rustc,
## rustfmt, nim, nimble, capnp). On Linux/macOS the Nix flake
## continues to supply the same toolchain. Either path produces
## byte-equivalent build outputs and the same test pass/fail set —
## CI cross-checks this through the side-by-side `ci.yml` (nix) +
## `ci-reprobuild.yml` (reprobuild) flow per Repo-Requirements §2.9.
##
## EVM: tests spawn anvil + invoke solc through Foundry.

import repro_project_dsl

package codetracer_evm_recorder:
  uses:
    # Rust toolchain — declared by version so the tarball-direct
    # provisioning entries in repro_dsl_stdlib/packages/cargo.nim /
    # rustc.nim / rustfmt.nim resolve on Windows. On Linux/macOS the
    # nix flake supplies the same versions.
    "rustc >=1.85"
    "cargo >=1.85"

    # Nim toolchain — codetracer_trace_writer_nim's build.rs compiles
    # a static library at cargo build time.
    "nim >=2.2 <3.0"
    "nimble"

    # Cap'n Proto schema compiler used by the recorder's build.rs.
    "capnp"

    # libzstd headers + library, needed when linking the Nim FFI
    # static library into the cargo build.
    "zstd"

    # pkg-config + OpenSSL — openssl-sys consults pkg-config to find
    # OpenSSL on Linux/macOS. The Windows build uses the rustls-tls
    # feature instead so neither is on the windows toolchain floor.
    when not defined(windows):
      "pkg-config"
      "openssl"

    # Language-specific compiler / runtime tools. ``solc`` compiles
    # the Solidity test fixtures; ``foundry`` (anvil + forge + cast)
    # spins up the local Ethereum node the integration tests drive
    # against.
    "solc"
    "foundry"

  executable codetracerEvmRecorder:
    name: "codetracer-evm-recorder"

  devEnv:
    activity "default"

  build:
    # ---- Primary build edge (the `default` collection) ----------------
    #
    # Native cargo build for the recorder binary. Enrolled into the
    # conventional ``default`` collection per
    # reprobuild-specs/Build-Graph-Collections.md §"`default`"; this
    # makes ``repro build`` (no positional target) materialise this
    # edge's closure.
    const binarySuffix = (when defined(windows): ".exe" else: "")
    const recorderBinary =
      "target/release/codetracer-evm-recorder" & binarySuffix

    # NB: ``locked = false`` because codetracer-evm-recorder's
    # ``.gitignore`` excludes ``Cargo.lock`` (the repo treats itself
    # as a library by convention). Cargo regenerates the lock file on
    # the first build; cross-repo sibling revisions are pinned by the
    # repo-workspaces workspace lock (resolved in CI via
    # ``scripts/resolve-sibling-rev.sh``) rather than a committed lock
    # file. Other recorders (cairo, fuel, leo, …) do check Cargo.lock
    # in and set ``locked = true`` to gate against accidental drift.
    let recorderBuild = cargo.build(
      release = true,
      actionId = "codetracer-evm-recorder.cargo-build",
      extraInputs = @[
        "Cargo.toml",
        "src", "build.rs"
      ],
      extraOutputs = @[recorderBinary])
    discard collect("default", @[recorderBuild])

    # ---- Test-binary build + run edges (the `test` collection) -------
    #
    # Two-stage shape per Repo-Requirements.md §2.8: `cargo.test(noRun =
    # true)` builds every cargo test binary into
    # `target/debug/deps/<crate>-<hash>` (the engine tracks the deps
    # directory as the build edge's effect set because the hashed
    # filename floats with input content); `cargo.test(noRun = false)`
    # then runs the binaries in one cargo invocation. The execute edge
    # depends on the build edge so the engine only re-runs tests when
    # an input changed since the last successful execution.
    #
    # Per-test execute edges fall out automatically once the
    # ct-test-runner cargo adapter lands per
    # reprobuild-specs/Test-Edges-And-Parallel-Runner.milestones.org
    # §M4 — the whole-binary edge becomes a fan-out point without
    # changing this recipe.

    let testsBuild = cargo.test(
      noRun = true,
      actionId = "codetracer-evm-recorder.cargo-test-build",
      extraInputs = @[
        "Cargo.toml",
        "src", "build.rs", "tests"
      ],
      extraOutputs = @["target/debug/deps"])

    let testsRun = cargo.test(
      actionId = "codetracer-evm-recorder.cargo-test-run",
      after = @[testsBuild.action],
      extraInputs = @[
        "Cargo.toml",
        "src", "tests",
        "target/debug/deps"
      ])

    discard collect("test", @[testsRun.action])
