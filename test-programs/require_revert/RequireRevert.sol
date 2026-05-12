// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title RequireRevert — exercise `require` and `revert` paths.
///
/// `run()` is the **happy path**: it calls `safe(true)` which passes
/// the require, writes a storage slot, and returns.  This is the
/// path the recorder test invokes through normal anvil dispatch.
///
/// `failingRequire()` and `failingRevert()` are stubs the test can
/// invoke separately to verify the recorder still produces a trace
/// when the EVM reverts — both with and without a revert string.
contract RequireRevert {
    uint256 public stored;

    event Ok(uint256 v);

    function run() public returns (uint256) {
        uint256 v = safe(true);
        stored = v;
        emit Ok(v);
        return v;
    }

    function safe(bool flag) internal returns (uint256) {
        require(flag, "must be true");
        return 7;
    }

    function failingRequire() public pure returns (uint256) {
        // require with reason string — emits a string-encoded revert.
        require(false, "always fails");
        return 0; // unreachable
    }

    function failingRevert() public pure returns (uint256) {
        // bare revert with no reason — emits an empty revert payload.
        revert();
    }
}
