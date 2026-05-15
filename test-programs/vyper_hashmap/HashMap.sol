// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title HashMap — Solidity substitute for the Vyper `HashMap[K, V]`
/// fixture.
///
/// Vyper is not available in the dev shell, so the
/// `vyper_hashmap_test` slot is filled by a Solidity fixture using
/// `mapping(address => uint256)` (Solidity's analogue of Vyper's
/// `HashMap[address, uint256]`) plus a nested
/// `mapping(address => mapping(address => uint256))` (Vyper's
/// `HashMap[address, HashMap[address, uint256]]`).
///
/// Mapping access in Solidity (and Vyper) lowers to
/// `keccak256(key . slot)` SLOAD/SSTORE pairs at the EVM level.  The
/// strict pin must surface the SSTOREs as step events and the read
/// values as a return event.
contract HashMap {
    mapping(address => uint256) public balances;
    mapping(address => mapping(address => uint256)) public allowances;

    event Sum(uint256 total);

    function run() public returns (uint256) {
        // Two top-level mapping writes (different keys → different slots).
        balances[address(0xAAA)] = 100;
        balances[address(0xBBB)] = 200;

        // Two nested mapping writes (same outer key → same inner-mapping
        // slot, different inner keys → different storage slots).
        allowances[address(0xAAA)][address(0xCCC)] = 7;
        allowances[address(0xAAA)][address(0xDDD)] = 11;

        // Read everything back through the public getter shape.
        uint256 total = balances[address(0xAAA)]
            + balances[address(0xBBB)]
            + allowances[address(0xAAA)][address(0xCCC)]
            + allowances[address(0xAAA)][address(0xDDD)];
        emit Sum(total);
        return total;
    }
}
