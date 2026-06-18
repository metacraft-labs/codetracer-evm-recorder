// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title MultiStmt - showcase column-aware step-over.
///
/// The body of `run()` packs three statements onto a single source line
/// so each starts at a distinct column (1-based: 9, 21, 33 inside the
/// indented body). A traditional line-only debugger would treat them
/// as a single step; CodeTracer's EVM recorder emits a distinct step
/// per column, so you can step-over `x = 1`, then `y = 2`, then
/// `z = 3` independently and watch each variable spring into
/// existence.
contract MultiStmt {
    function run() public pure returns (uint256) {
        uint x = 1; uint y = 2; uint z = 3;
        return x + y + z;
    }
}
