// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Assembly — exercise inline `assembly { ... }` blocks.
///
/// Solidity inline assembly compiles to Yul, which then compiles to a
/// raw sequence of EVM opcodes inlined into the surrounding function's
/// bytecode.  Each Yul statement carries its own source map entry, so
/// the recorder must surface step events whose line numbers land
/// INSIDE the assembly block (NOT collapsed to the enclosing function
/// header).
///
/// `run()` performs three operations via inline assembly:
///
///   1. arithmetic — `add` of two 256-bit words,
///   2. SSTORE — write to storage slot 0 directly,
///   3. SLOAD — read it back into a local that the Solidity layer
///      then emits via `Result(uint256)`.
///
/// The strict pin asserts:
///
///   * step events surface with line numbers inside the asm block
///     (line 33 — the `let sum := add(a, b)` line), proving the
///     source map propagates through Yul,
///   * exactly one `Result(uint256)` event with the computed value
///     (`a + b = 7 + 11 = 18 = 0x12`),
///   * the storage slot 0 carry-forward variable surfaces with the
///     SSTORE'd value as a typed ValueRecord.
contract Assembly {
    uint256 public stored;

    event Result(uint256 v);

    function run() public returns (uint256) {
        uint256 a = 7;
        uint256 b = 11;
        uint256 result;
        assembly {
            let sum := add(a, b)
            sstore(0, sum)
            result := sload(0)
        }
        emit Result(result);
        return result;
    }
}
