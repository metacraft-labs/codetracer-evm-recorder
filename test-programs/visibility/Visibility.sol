// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Visibility — exercise the four Solidity visibility modifiers.
///
/// Single contract carrying one function per visibility level
/// (`public` / `external` / `internal` / `private`) plus a
/// parameterless `caller()` entry-point that calls each of them.
///
/// The CLI invokes `caller()`; inside that frame:
///
///   * `publicFn()` and `externalFn()` are dispatched via
///     `this.publicFn()` / `this.externalFn()` so they go through the
///     dispatcher as ordinary EXTERNAL calls (CALL opcode → depth +1).
///   * `internalFn()` and `privateFn()` are reached via JUMP at the
///     same call depth as `caller` — solc inlines `internal` /
///     `private` calls into the caller's bytecode.
///
/// The strict pin asserts:
///   * the function table contains all four visibility-bearing names
///     (`publicFn`, `externalFn`, `internalFn`, `privateFn`) plus
///     `caller`,
///   * the call sequence visits two cross-contract `external_call_*`
///     placeholders (one per `this.*` invocation),
///   * the IO event payload encodes the cumulative accumulator
///     `1 + 2 + 4 + 8 = 15 = 0xf`.
contract Visibility {
    event Result(uint256 v);

    function publicFn() public pure returns (uint256) {
        return 1;
    }

    function externalFn() external pure returns (uint256) {
        return 2;
    }

    function internalFn() internal pure returns (uint256) {
        return 4;
    }

    function privateFn() private pure returns (uint256) {
        return 8;
    }

    function caller() public returns (uint256) {
        uint256 a = this.publicFn();
        uint256 b = this.externalFn();
        uint256 c = internalFn();
        uint256 d = privateFn();
        uint256 total = a + b + c + d;
        emit Result(total);
        return total;
    }
}
