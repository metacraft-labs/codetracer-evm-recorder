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
## `shell(command = "bash scripts/...")` wrappers for the Rust build /
## test — delegation defeats the engine's incremental-build,
## action-cache, per-test invalidation, and the CI sharding the engine
## grows into per ``reprobuild-specs/CI-Sharding.md``. The ONE
## ``sh.shell`` edge below wraps the repo's CLI-convention verification
## script, which is not a cargo target — it is a POSIX-shell assertion
## harness that ``just test`` runs after ``cargo test`` (see ``Justfile``
## ``test:``), so it is modelled as its own execute edge rather than
## dropped.
##
## On Windows the recipe drives real reprobuild tool provisioning via
## the tarball entries the ``uses:`` packages declare (cargo, rustc,
## nim, nimble, capnp). On Linux/macOS the Nix flake continues to
## supply the same toolchain. Either path produces byte-equivalent
## build outputs and the same test pass/fail set — CI cross-checks this
## through the side-by-side `ci.yml` (nix) + `ci-reprobuild.yml`
## (reprobuild) flow per Repo-Requirements §2.9.
##
## **This repo is a Rust CONSUMER of two sibling crates, but NOT a
## reprobuild ``uses: "<sibling>"`` consumer.** The recorder's
## ``Cargo.toml`` pulls in two crates from the sibling
## ``codetracer-trace-format`` repo via cargo ``path`` dependencies:
## ``codetracer_trace_types`` and ``codetracer_trace_writer_nim``. Both
## are resolved and compiled INSIDE cargo — out of reprobuild's reach —
## so they are NOT reprobuild library-threaded ``uses:`` consumptions
## (the SC-11 develop-mode src-threading applies only to reprobuild's
## own ``nim.c`` edges, and ``codetracer-trace-format`` is a Rust
## workspace, not a Nim-library sibling in the AVAILABLE set). This
## matches how the sibling recorders (circom, cairo, …) model the
## identical dependency: the toolchain floor for the Nim FFI that
## ``codetracer_trace_writer_nim``'s ``build.rs`` compiles at cargo
## build time (``nim`` + ``nimble`` + ``capnp`` + ``zstd``) is declared
## in ``uses:``, and cargo does the cross-crate wiring itself. The lock
## below is therefore self-only.
##
## **Per-test platform gating.** ``just test`` is ``cargo test``
## followed by the CLI-convention shell script — no test FILE in this
## repo carries a per-host gate. The ``tests/*.rs`` integration tests
## compile ``.sol`` fixtures via the pinned ``solc`` and drive a local
## ``anvil`` node from ``foundry`` on every host, so the single
## whole-workspace ``cargo.test`` execute edge below matches the repo's
## own ``just test`` one-for-one — there is no per-OS partition to
## model. The shell verify edge is POSIX-portable (``bash``) and is
## likewise unconditional.
##
## **Tool provisioning.** ``defaultToolProvisioning "path"`` matches the
## canonical Rust-recorder recipes: the nix dev shell puts ``cargo`` /
## ``rustc`` / ``nim`` / ``nimble`` / ``capnp`` / ``solc`` / ``anvil`` /
## ``zstd`` on ``PATH`` (and ``PKG_CONFIG_PATH`` for libzstd + openssl),
## so the weak-local PATH resolver is the right default. Without it
## ``repro build`` refuses to run with "typed tool provisioning is
## required for uses declarations".
##
## EVM: tests spawn anvil + invoke solc through Foundry.

import repro_project_dsl
import repro_dsl_stdlib/packages/sh

package codetracer_evm_recorder:
  defaultToolProvisioning "path"

  uses:
    # Rust toolchain — declared by version so the tarball-direct
    # provisioning entries in repro_dsl_stdlib/packages/cargo.nim /
    # rustc.nim resolve on Windows. On Linux/macOS the nix flake
    # supplies the same versions.
    "rustc >=1.85"
    "cargo >=1.85"

    # Nim toolchain — the sibling ``codetracer_trace_writer_nim`` crate's
    # build.rs compiles a Nim FFI static library at cargo build time via
    # ``nim c``; ``nimble`` resolves that FFI's nimble requirements.
    "nim >=2.2 <3.0"
    "nimble"

    # Cap'n Proto schema compiler used by the trace-format crates'
    # build.rs (``capnpc`` over the trace schema).
    "capnp"

    # libzstd headers + library, needed when linking the Nim FFI
    # static library into the cargo build (the FFI's C output
    # ``#include``s ``zstd.h`` and the CBOR+Zstd writer links libzstd).
    "zstd"

    # pkg-config + OpenSSL — openssl-sys consults pkg-config to find
    # OpenSSL on Linux/macOS. The Windows build uses the rustls-tls
    # feature instead so neither is on the windows toolchain floor.
    when not defined(windows):
      "pkg-config"
      "openssl"

    # POSIX shell — drives the CLI-convention verification edge below,
    # the same ``bash tests/verify-cli-convention-no-silent-skip.sh``
    # step ``just test`` runs after ``cargo test``.
    "sh"

    # Language-specific compiler / runtime tools. ``solc`` compiles the
    # Solidity test fixtures the integration tests record against;
    # ``foundry`` (anvil + forge + cast) spins up the local Ethereum
    # node the integration tests drive against.
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
    #
    # NB: ``locked = false`` because codetracer-evm-recorder's
    # ``.gitignore`` excludes ``Cargo.lock`` (the repo treats itself as a
    # library by convention). Cargo regenerates the lock file on the
    # first build; cross-repo sibling revisions are pinned by the
    # repo-workspaces workspace lock rather than a committed lock file.
    # Other recorders (circom, cairo, fuel, …) DO check Cargo.lock in and
    # set ``locked = true`` to gate against accidental drift.
    #
    # The recorder has no ``build.rs`` of its own; the only inputs are
    # the manifest and the ``src`` tree. The sibling trace-format crates
    # cargo pulls in via ``path`` deps are tracked per-crate at
    # action-end by cargo's own ``.d`` depfiles under ``target/*/deps``.
    const binarySuffix = (when defined(windows): ".exe" else: "")
    const recorderBinary =
      "target/release/codetracer-evm-recorder" & binarySuffix

    let recorderBuild = cargo.build(
      release = true,
      actionId = "codetracer-evm-recorder.cargo-build",
      extraInputs = @[
        "Cargo.toml",
        "src"
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
        "src", "tests", "test-programs", "contracts"
      ],
      extraOutputs = @["target/debug/deps"])

    let testsRun = cargo.test(
      actionId = "codetracer-evm-recorder.cargo-test-run",
      after = @[testsBuild.action],
      extraInputs = @[
        "Cargo.toml",
        "src", "tests", "test-programs", "contracts",
        "target/debug/deps"
      ])

    # ---- CLI-convention verification edge -----------------------------
    #
    # ``just test`` runs ``bash
    # tests/verify-cli-convention-no-silent-skip.sh`` after ``cargo
    # test``. The script asserts the recorder's ``--help`` / ``--version``
    # surface complies with ``Recorder-CLI-Conventions.md``. It is not a
    # cargo target, so it is modelled as its own ``sh.shell`` execute
    # edge rather than dropped — reproducing the repo's full ``just test``
    # set. The script builds the debug binary and inspects its ``--help``
    # text, so it is re-run every ``repro test`` pass (matching ``just
    # test``); ``after`` the cargo test-build edge guarantees the binary
    # exists before the script runs.
    let cliVerify = shell(
      command = "bash tests/verify-cli-convention-no-silent-skip.sh",
      actionId = "codetracer-evm-recorder.verify-cli-convention",
      after = @[testsBuild.action],
      extraInputs = @[
        "tests/verify-cli-convention-no-silent-skip.sh",
        "Cargo.toml", "src"
      ],
      cacheable = false)

    discard collect("test", @[testsRun.action, cliVerify])
