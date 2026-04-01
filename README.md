## codetracer-evm-recorder

A recorder of EVM/Solidity smart contract executions that produces [CodeTracer](https://github.com/metacraft-labs/CodeTracer) traces.

> [!WARNING]
> Currently it is in a very early phase: we're welcoming contribution and discussion!

### Overview

codetracer-evm-recorder compiles Solidity programs with solc, deploys them to a local Anvil node, executes a target function, and captures `debug_traceTransaction` structlogs. It resolves source mappings and reconstructs variable values from the EVM stack, emitting structured trace files compatible with CodeTracer.

### Building

Requires `solc` and `anvil`. The recommended way to obtain them is via the Nix dev shell:

```bash
nix develop
cargo build
```

### Usage

Record a trace from a Solidity source file:

```bash
codetracer-evm-recorder record <solidity-file> --trace-dir <dir> [--function <name>]
# Produces trace files in <dir>.
# --function selects the entry point to trace (defaults to the first public function).
```

> **Note:** This recorder currently uses `--trace-dir` (not `--out-dir`) and `--function`
> (not `--format`). These flags may be harmonized with the other recorders in a future release.

Replay an on-chain transaction:

```bash
codetracer-evm-recorder replay <tx-hash> --trace-dir <dir>
```

However, you probably want to use it in combination with CodeTracer, which would be released soon.

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
```

### Environment variables

* `RUST_LOG` — controls log verbosity (standard `env_logger` syntax, e.g. `RUST_LOG=debug`)

### Contributing

We'd be very happy if the community finds this useful, and if anyone wants to:

* Use and test the EVM/Solidity support or CodeTracer.
* Provide feedback and discuss alternative implementation ideas: in the issue tracker, or in our [discord](https://discord.gg/qSDCAFMP).
* Contribute code to enhance the EVM/Solidity support of CodeTracer.
* Provide [sponsorship](https://opencollective.com/codetracer), so we can hire dedicated full-time maintainers for this project.

### Legal info

LICENSE: MIT

Copyright (c) 2025 Metacraft Labs Ltd
