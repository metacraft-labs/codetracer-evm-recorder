// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Library — exercise `library` declarations + `using ... for`.
///
/// `library SafeMath` exposes two pure helpers (`add`, `mul`).  The
/// `Library` contract binds them to `uint256` via `using SafeMath for
/// uint256`, so the call sites read like method calls on the receiver
/// (`x.add(y).mul(2)`).
///
/// Solidity inlines `internal` library functions directly into the
/// caller's bytecode (no DELEGATECALL), so the trace must surface them
/// as ordinary internal call frames — not as cross-contract calls.
/// The strict assertions verify:
///
///   * the function table contains both `add` and `mul` under their
///     library-resolved AST names,
///   * `compute(5, 10)` walks add → mul (15 → 30) and emits a single
///     `Result(30)` event,
///   * NO `external_call_depth_2` placeholder appears (would indicate
///     the recorder mistakenly treated the JUMP as a CALL).
library SafeMath {
    function add(uint256 a, uint256 b) internal pure returns (uint256) {
        return a + b;
    }

    function mul(uint256 a, uint256 b) internal pure returns (uint256) {
        return a * b;
    }
}

contract Library {
    using SafeMath for uint256;

    event Result(uint256 v);

    function compute(uint256 x, uint256 y) public pure returns (uint256) {
        return x.add(y).mul(2);
    }

    function run() public returns (uint256) {
        uint256 v = compute(5, 10);
        emit Result(v);
        return v;
    }
}
