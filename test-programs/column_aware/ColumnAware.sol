// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title ColumnAware - exercise multi-statement-per-line column tracking.
///
/// `run()` packs three `uint` declarations onto a single source line so
/// each statement starts at a distinct column (1-based: 9, 21, 33 inside
/// the indented body).  Under column-aware navigation the recorder must
/// surface a step for each statement with strictly distinct column
/// values; without column awareness all three collapse onto the same
/// `(line, column=1)` pair and only the first surfaces as a step.
///
/// See `codetracer-specs/Planned-Features/
/// Column-Aware-Navigation-Other-Languages.plan.md` and the JS reference
/// fixture at
/// `codetracer-js-recorder/tests/integration/column-aware.test.ts`.
contract ColumnAware {
    function run() public pure returns (uint256) {
        uint x = 1; uint y = 2; uint z = 3;
        return x + y + z;
    }
}
