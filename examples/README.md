# CodeTracer EVM Examples

This directory holds small Solidity programs you can record and replay
in CodeTracer to learn the workflow. Each `.sol` file is a self-contained
contract whose `run()` function performs a few computations chosen to
make the recorded trace easy to read in the GUI.

The fixtures named after a contract (`FlowTest.sol`, `ControlFlow.sol`,
`StorageOps.sol`, `ERC20.sol`) are symlinks into [`../test-programs/`](../test-programs/)
so they stay in sync with the recorder's own test suite and never drift
from a fixture the recorder is known to handle. The only standalone
example is [`multi_stmt.sol`](./multi_stmt.sol), which exists solely to
showcase column-aware step-over.

## Prerequisites

1. A recent CodeTracer build with the `ct` launcher on your `PATH`.
   Verify with:

   ```bash
   ct --version
   ```

   See the [main CodeTracer repo](https://github.com/metacraft-labs/CodeTracer)
   for installation instructions.

2. This recorder built and discoverable. From the repo root:

   ```bash
   nix develop      # drops you into a shell with solc + anvil
   cargo build      # builds codetracer-evm-recorder
   ```

   `ct record` invokes the recorder binary on your behalf — you should
   not need to call it directly. If `ct` cannot find it, make sure the
   `cargo build` output (e.g. `target/debug/codetracer-evm-recorder`) is
   reachable via your CodeTracer installation's recorder discovery path.

## Two-step workflow: record, then replay

Use this when you want to keep the trace around, share it, or replay it
multiple times.

```bash
# 1. Record. Produces a .ct CTFS bundle in the chosen output folder.
ct record examples/FlowTest.sol

# 2. Replay. Opens the trace folder in the CodeTracer GUI.
ct replay -t <path-printed-by-ct-record>
```

`ct record` prints the location of the trace folder it produced; pass
that path to `ct replay -t`. You can also list previous recordings
interactively by running `ct replay -i`.

## One-step workflow: record and open immediately

For quick iteration, `ct run` records the program and opens the trace in
the GUI in a single command:

```bash
ct run examples/FlowTest.sol
```

This is the recommended way to explore the examples below.

## Walkthrough: `multi_stmt.sol`

We pick [`multi_stmt.sol`](./multi_stmt.sol) because it demonstrates the
EVM recorder's **column-aware step-over** — the headline navigation
feature that distinguishes CodeTracer from line-only Solidity debuggers.

The contract:

```solidity
contract MultiStmt {
    function run() public pure returns (uint256) {
        uint x = 1; uint y = 2; uint z = 3;
        return x + y + z;
    }
}
```

Run it:

```bash
ct run examples/multi_stmt.sol
```

In the GUI you should see:

- **Call tree** — a single entry for `MultiStmt.run`, the function the
  recorder selects automatically (it defaults to `run`).
- **Source view** — `multi_stmt.sol` with an execution cursor parked on
  the first statement of `run()`.
- **Variables panel** — initially empty inside `run()`. As you step, `x`,
  then `y`, then `z` appear with their literal values.
- **Step Over (F10)** — *here is the key bit:* even though `x`, `y`, and
  `z` are declared on the same source line, the recorder emits a
  separate step event for each statement. Pressing `F10` advances one
  statement at a time, with the cursor visibly hopping across the
  columns `9`, `21`, `33` of that line. A traditional line-only debugger
  would jump straight from the declarations to `return x + y + z`,
  skipping `y` and `z`'s side effects entirely.
- **Step Back (Shift+F10)** — reverse the same motion to confirm the
  recorder is genuinely tracking column positions, not animating.

## What else is here

Once you are comfortable with column-aware navigation, the symlinked
fixtures cover progressively richer recorder features:

- [`FlowTest.sol`](./FlowTest.sol) — straight-line data flow with
  intermediate sums and two storage writes.
- [`ControlFlow.sol`](./ControlFlow.sol) — `if`/`else`, `while`, and
  `for`, plus a `Done(uint256)` event.
- [`StorageOps.sol`](./StorageOps.sol) — three named storage slots
  written and read back so the trace contains SSTORE + SLOAD pairs.
- [`ERC20.sol`](./ERC20.sol) — minimal ERC-20-shaped fixture
  exercising mappings, allowances, and `Transfer`/`Approval` events.

Any `.sol` file under [`../test-programs/`](../test-programs/) can be
fed to `ct record` or `ct run` the same way — that directory is the
authoritative library of recorder-verified Solidity programs covering
modifiers, `try`/`catch`, inheritance, libraries, assembly, and more.

Read the top-level [`README.md`](../README.md) for an architectural
overview of the recorder and the CTFS trace format it emits.
