// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title ControlFlow — exercise if/else, while, and for loops.
///
/// `run()` is the canonical entry point for the recorder test suite.
/// The function deterministically exercises three control-flow shapes
/// in sequence, asserting on intermediate sums so the resulting trace
/// has predictable counts and decoded values:
///
///   - if/else: branch on `flag = true` → `branchVal = 100`.
///   - while:   accumulate `total` over `counter = 0..3` → `whileSum = 6`.
///   - for:     accumulate `total` over `i = 1..5` → `forSum = 15`.
///
/// The function emits a single `Done(uint256)` event with the grand
/// total `(branchVal + whileSum + forSum) = 121` and writes that
/// value to the `result` storage slot, so the trace has at least one
/// SSTORE and one LOG opcode for the recorder to surface.
contract ControlFlow {
    uint256 public result;

    event Done(uint256 total);

    function run() public returns (uint256) {
        // --- if/else ---
        bool flag = true;
        uint256 branchVal;
        if (flag) {
            branchVal = 100;
        } else {
            branchVal = 200;
        }

        // --- while loop: 3 iterations (counter = 0,1,2,3) ---
        uint256 whileSum = 0;
        uint256 counter = 0;
        while (counter < 3) {
            whileSum = whileSum + counter;
            counter = counter + 1;
        }
        // whileSum == 0 + 1 + 2 == 3; loop body executes 3 times.

        // --- for loop: 5 iterations (i = 1..5) ---
        uint256 forSum = 0;
        for (uint256 i = 1; i <= 5; i++) {
            forSum = forSum + i;
        }
        // forSum == 1 + 2 + 3 + 4 + 5 == 15; loop body executes 5 times.

        uint256 total = branchVal + whileSum + forSum; // 100 + 3 + 15 == 118
        result = total;
        emit Done(total);
        return total;
    }
}
