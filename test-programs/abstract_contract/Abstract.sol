// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Abstract — exercise `abstract contract` + virtual functions.
///
/// `abstract contract Foo { function bar() public virtual returns
/// (uint256); }` declares a virtual method without a body — the
/// `Foo` contract itself can never be deployed (solc rejects deploying
/// an abstract contract because it has no bytecode for `bar`).  The
/// concrete subclass `Abstract is Foo` provides the implementation.
///
/// `run()` invokes `bar()` through `this` (a `Foo` reference at
/// compile time, but at runtime resolves to `Abstract.bar` via the
/// virtual dispatch JUMP table).  Because `super` is not in play and
/// the call is made through a same-contract internal JUMP, no
/// DELEGATECALL or CALL opcode is emitted — the trace must surface
/// `Abstract.bar` as a normal internal call frame.
///
/// The strict pin asserts:
///
///   * the function table contains the AST-resolved subclass override
///     `bar` (NOT the abstract base's bodyless declaration, which has
///     no source position past its `;` and would resolve to a
///     `fn_at_pc_*` placeholder if anything),
///   * exactly one `bar` call_entry/exit pair surfaces (proving the
///     virtual dispatch landed on the subclass implementation),
///   * the `Result(uint256)` event encodes the concrete return value.
abstract contract Foo {
    function bar() public virtual returns (uint256);
}

contract Abstract is Foo {
    event Result(uint256 v);

    function bar() public override returns (uint256) {
        return 42;
    }

    function run() public returns (uint256) {
        uint256 v = bar();
        emit Result(v);
        return v;
    }
}
