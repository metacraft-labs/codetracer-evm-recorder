// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title ControlFlowTest — Exercises branching, loops, and internal calls.
///
/// Designed to test local variable tracking through:
///   - if/else branches where different variables are set in each arm
///   - for loops with a counter and accumulator
///   - nested internal function calls
///   - multiple return values from an internal function
contract ControlFlowTest {
    uint256 public lastResult;

    event BranchTaken(bool tookTrueBranch);
    event LoopDone(uint256 total);

    /// Execute branching logic.
    ///
    /// Expected variable values when `flag == true`:
    ///   x = 100, y = 0 (unchanged), branchVal = 42
    /// Expected variable values when `flag == false`:
    ///   x = 0 (unchanged), y = 200, branchVal = 99
    function branching(bool flag) public returns (uint256) {
        uint256 x = 0;
        uint256 y = 0;
        uint256 branchVal = 0;

        if (flag) {
            x = 100;
            branchVal = 42;
        } else {
            y = 200;
            branchVal = 99;
        }

        lastResult = branchVal;
        emit BranchTaken(flag);
        return branchVal;
    }

    /// Execute a for loop summing 1..n.
    ///
    /// Expected variable values after loop (n=5):
    ///   total = 15 (1+2+3+4+5)
    ///   i = 6 (loop counter after exit)
    function looping(uint256 n) public returns (uint256) {
        uint256 total = 0;

        for (uint256 i = 1; i <= n; i++) {
            total = total + i;
        }

        lastResult = total;
        emit LoopDone(total);
        return total;
    }

    /// Nested internal calls with multiple return values.
    ///
    /// Expected variable values:
    ///   sum = a + b
    ///   product = a * b
    ///   combined = sum + product
    function nestedCalls(uint256 a, uint256 b) public returns (uint256) {
        (uint256 sum, uint256 product) = addAndMultiply(a, b);
        uint256 combined = addPure(sum, product);

        lastResult = combined;
        return combined;
    }

    /// Internal pure function returning two values.
    function addAndMultiply(uint256 x, uint256 y)
        internal
        pure
        returns (uint256, uint256)
    {
        uint256 s = x + y;
        uint256 p = x * y;
        return (s, p);
    }

    /// Simple internal pure addition (for nesting test).
    function addPure(uint256 x, uint256 y) internal pure returns (uint256) {
        return x + y;
    }
}
