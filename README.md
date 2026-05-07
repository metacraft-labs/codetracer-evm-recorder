## codetracer-evm-recorder

A recorder of EVM/Solidity smart contract executions that produces [CodeTracer](https://github.com/metacraft-labs/CodeTracer) traces.

> [!WARNING]
> Currently it is in a very early phase: we're welcoming contribution and discussion!

### Overview

codetracer-evm-recorder compiles Solidity programs with solc, deploys them to a local Anvil node, executes a target function, and captures `debug_traceTransaction` structlogs. It resolves source mappings and reconstructs variable values from the EVM stack, emitting a CodeTracer multi-stream CTFS bundle compatible with the rest of CodeTracer.

The recorder is CTFS-only — see [`Recorder-CLI-Conventions.md`](https://github.com/metacraft-labs/codetracer-specs/blob/main/Recorder-CLI-Conventions.md) §4. To convert a recorded `.ct` bundle to JSON or text for inspection, use `ct print` from [`codetracer-trace-format-nim`](https://github.com/metacraft-labs/codetracer-trace-format-nim); the recorder itself never produces these forms.

### Building

Requires `solc` and `anvil`. The recommended way to obtain them is via the Nix dev shell:

```bash
nix develop
cargo build
```

### Usage

Record a trace from a Solidity source file:

```bash
codetracer-evm-recorder record <solidity-file> --out-dir <dir> [--function <name>]
# Produces a CTFS .ct bundle in <dir>.
# --function selects the entry point to trace (defaults to `run`, or the first
# non-constructor function if `run` is not present).
```

Replay an on-chain transaction:

```bash
codetracer-evm-recorder replay <tx-hash> --out-dir <dir>
```

> **Deprecated:** `--trace-dir` is still accepted as a legacy alias for `--out-dir` so existing scripts keep working. It emits a one-line stderr deprecation note (`warning: --trace-dir is deprecated, use --out-dir`) and will be removed in a future release. New callers should use `--out-dir` / `-o`.

### Inspecting a recorded trace

The recorder writes a `.ct` CTFS bundle. To dump it as JSON for debugging or golden-snapshot tests:

```bash
ct print --json <dir>/<Contract>.ct
```

`ct print` is shipped with `codetracer-trace-format-nim` (sibling repository).

### Architecture

The recorder is organized into the following modules:

* `recorder.rs` — top-level recording orchestration and trace file output
* `inspector.rs` — EVM opcode inspection and step-level tracing
* `source_map.rs` — solc source mapping resolution
* `solidity_ast.rs` — Solidity AST parsing for function and variable discovery
* `stack_tracker.rs` — EVM stack analysis to recover variable values
* `structlog.rs` — `debug_traceTransaction` structlog parsing
* `call_tree.rs` — call graph reconstruction from trace data
* `contract_registry.rs` — deployed contract address tracking
* `storage_layout.rs` — contract storage layout resolution
* `source_fetcher.rs` — verified source code retrieval for on-chain contracts
* `trace_fetcher.rs` — remote trace data retrieval for replay
* `replay.rs` — on-chain transaction replay

### Testing

Test programs live in `test-programs/`. Run the test suite with:

```bash
cargo test
# or, via just:
just test
```

`just test` additionally runs `tests/verify-cli-convention-no-silent-skip.sh`, a bash-level guard that asserts the CLI continues to comply with `Recorder-CLI-Conventions.md` (no silent regressions of `--out-dir`, `--version`, `ct print` mention, or the `CODETRACER_EVM_RECORDER_*` env-var fallbacks).

### Environment variables

Convention: `Recorder-CLI-Conventions.md` §5.

| Variable                            | CLI equivalent | Description                                                                                                 |
| ----------------------------------- | -------------- | ----------------------------------------------------------------------------------------------------------- |
| `CODETRACER_EVM_RECORDER_OUT_DIR`   | `--out-dir`    | Output directory for traces. Falls back here when the CLI flag is omitted; the CLI flag always wins.        |
| `CODETRACER_EVM_RECORDER_DISABLED`  | —              | Set to `1` or `true` to skip recording entirely. The recorder still validates inputs but does not spin up Anvil or write any trace artefacts. |
| `RUST_LOG`                          | —              | Recorder log verbosity (standard `env_logger` syntax, e.g. `RUST_LOG=debug`).                               |

### Contributing

We'd be very happy if the community finds this useful, and if anyone wants to:

* Use and test the EVM/Solidity support or CodeTracer.
* Provide feedback and discuss alternative implementation ideas: in the issue tracker, or in our [discord](https://discord.gg/qSDCAFMP).
* Contribute code to enhance the EVM/Solidity support of CodeTracer.
* Provide [sponsorship](https://opencollective.com/codetracer), so we can hire dedicated full-time maintainers for this project.

### Legal info

LICENSE: MIT

Copyright (c) 2025 Metacraft Labs Ltd
