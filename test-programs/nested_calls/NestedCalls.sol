// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title NestedCalls — exercise a 4-deep internal call chain.
///
/// The chain `run -> outer -> middle -> inner` exercises Solidity
/// internal function dispatch via JUMP/JUMPDEST.  The recorder is
/// expected to surface each frame as a `call_entry` / `call_exit`
/// event in the trace, in order.  The leaf returns `1 + 2 = 3`,
/// `middle` adds `10`, `outer` adds `100`, and `run` adds the entry
/// `seed = 1000`, giving a final result of `1113`.
contract NestedCalls {
    uint256 public stored;

    event Result(uint256 r);

    function run() public returns (uint256) {
        uint256 seed = 1000;
        uint256 v = outer();
        uint256 r = seed + v;
        stored = r;
        emit Result(r);
        return r;
    }

    function outer() internal pure returns (uint256) {
        uint256 m = middle();
        return m + 100;
    }

    function middle() internal pure returns (uint256) {
        uint256 i = inner();
        return i + 10;
    }

    function inner() internal pure returns (uint256) {
        uint256 a = 1;
        uint256 b = 2;
        return a + b;
    }
}
