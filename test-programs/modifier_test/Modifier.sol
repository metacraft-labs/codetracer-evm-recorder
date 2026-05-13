// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Modifier — exercise the `modifier onlyOwner` pattern.
///
/// `run()` is the parameterless entry the recorder CLI invokes.  It
/// stages the canonical access-control flow end-to-end in a single
/// happy-path call:
///
///   1. The constructor sets `owner = msg.sender`.
///   2. `run()` invokes `setValue(7)`, guarded by `onlyOwner`.
///      Because the caller is the deployer (== `owner`), the
///      `require(msg.sender == owner, "not owner")` in the modifier
///      body passes and execution falls through the `_;` placeholder
///      into the wrapped function body.
///   3. `setValue` writes `value = 7` and emits `ValueSet(7)`.
///   4. `run()` reads `value` back and returns it.
///
/// Modifiers are *syntactic*: solc inlines the modifier body around
/// the wrapped function body at compile time.  We therefore expect
/// the require check and the wrapped body to show up as steps on
/// **distinct** source lines (the modifier's `require` line, then
/// the wrapped function's body line), even though there is no
/// dedicated `modifier` call frame in the trace.
///
/// The failing path (a non-owner caller hitting the `require`) is
/// covered by the M9-deferred `_failing_path_emits_error_event`
/// sibling pin in
/// `tests/test_programs_via_ct_print_full.rs`.
contract Modifier {
    address public owner;
    uint256 public value;

    event ValueSet(uint256 v);

    modifier onlyOwner() {
        require(msg.sender == owner, "not owner");
        _;
    }

    constructor() {
        owner = msg.sender;
    }

    function run() public returns (uint256) {
        setValue(7);
        return value;
    }

    function setValue(uint256 v) public onlyOwner {
        value = v;
        emit ValueSet(v);
    }

    function readValue() public view returns (uint256) {
        return value;
    }
}
