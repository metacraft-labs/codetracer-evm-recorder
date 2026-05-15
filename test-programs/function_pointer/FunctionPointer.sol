// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title FunctionPointer — exercise external function pointers.
///
/// Solidity supports first-class external function pointers of the
/// form `function (uint) external returns (uint)` — a (address,
/// selector) pair stored in a single 24-byte slot.  Calling through
/// such a pointer compiles to a regular external CALL targeting the
/// stored address with the stored selector.
///
/// The fixture has two contracts:
///
///   * `Target` — exposes a single external function `square(uint)`
///     returning `x * x`.  Deployed by the entry-point contract via
///     `new Target()`.
///   * `FunctionPointer` — the entry-point.  Stores a pointer to
///     `target.square` in `f`, then invokes it via `f(7)` and emits
///     the result via `Result(uint256)`.
///
/// Strict pin: the pointer invocation surfaces as an external CALL
/// (an `external_call_depth_2` placeholder frame), and the
/// `Result(uint256)` event encodes `7 * 7 = 49 = 0x31`.
contract Target {
    function square(uint256 x) external returns (uint256) {
        return x * x;
    }
}

contract FunctionPointer {
    Target public target;
    function (uint256) external returns (uint256) public f;

    event Result(uint256 v);

    function run() public returns (uint256) {
        target = new Target();
        f = target.square;
        uint256 v = f(7);
        emit Result(v);
        return v;
    }
}
