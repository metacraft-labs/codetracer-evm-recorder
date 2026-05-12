// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title StorageOps — exercise SSTORE + SLOAD round-trips.
///
/// `run()` writes three named storage slots, then reads them back
/// into local variables.  This guarantees the trace contains at
/// least three SSTORE opcodes (slots 0,1,2) and three SLOAD opcodes
/// from the read-back, so the recorder must surface all three storage
/// variables in its decoded value stream.
contract StorageOps {
    uint256 public a;
    uint256 public b;
    uint256 public c;

    event Stored(uint256 a, uint256 b, uint256 c);

    function run() public returns (uint256) {
        // --- writes: 3 SSTOREs ---
        a = 10;
        b = 20;
        c = 30;

        // --- reads: 3 SLOADs into locals ---
        uint256 ra = a;
        uint256 rb = b;
        uint256 rc = c;

        emit Stored(ra, rb, rc);
        return ra + rb + rc; // 60
    }
}
