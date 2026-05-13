// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title DelegateCall — proxy-pattern invariants under DELEGATECALL.
///
/// The recorder CLI deploys exactly one entry-point contract, so we
/// keep both halves of the proxy pattern in this single file:
///
///   - `Impl` is the logic contract: a tiny function that writes its
///     first storage slot.
///   - `DelegateCall` is the *fixture* entry-point.  In `run()` it
///     deploys an `Impl` instance (CREATE inside the same tx),
///     constructs the calldata for `Impl.setStored(uint256)`, and
///     `delegatecall`s into it.  Because DELEGATECALL preserves the
///     caller's storage context, the SSTORE the `Impl.setStored`
///     body executes lands on **this** contract's slot 0
///     (`stored` in `DelegateCall`'s layout), not on the freshly
///     deployed `Impl`'s storage.  That is the canonical
///     proxy-pattern invariant the recorder must capture.
///
/// The strict pin asserts:
///   - At least one DELEGATECALL frame appears in the call tree.
///   - The `stored` value at the end is 42, written through the
///     delegate-call.
///   - The `Result(uint256)` event surfaces as an EvmEvent io_event.

contract Impl {
    uint256 public stored;

    function setStored(uint256 v) public {
        stored = v;
    }
}

contract DelegateCall {
    uint256 public stored;
    address public impl;

    event Result(uint256 v);

    function run() public returns (uint256) {
        Impl i = new Impl();
        impl = address(i);

        // Build calldata for `setStored(uint256)` with v = 42.
        bytes memory data = abi.encodeWithSignature("setStored(uint256)", uint256(42));

        (bool ok, ) = address(i).delegatecall(data);
        require(ok, "delegatecall failed");

        emit Result(stored);
        return stored;
    }
}
