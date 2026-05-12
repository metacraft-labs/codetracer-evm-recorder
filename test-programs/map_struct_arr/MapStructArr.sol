// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title MapStructArr — exercise mappings, fixed-size arrays, and structs.
///
/// `run()` writes:
///   - one mapping entry        (`balances[caller] = 100`)
///   - one fixed-size array fill (`slots = [1, 2, 3]`)
///   - one struct field group   (`record = Record{id:7, value:42, active:true}`)
///
/// All three surface as SSTORE opcodes the recorder must capture.
/// A spec-compliant recorder would also expose the in-memory array
/// `slots` and struct `record` as `ValueRecord::Sequence` /
/// `ValueRecord::Struct` decoded values; the EVM recorder currently
/// emits them all as `Raw` u256 storage slots — see the
/// `_value_kinds_present` ignored sibling test for the spec-compliant
/// expectation.
contract MapStructArr {
    mapping(address => uint256) public balances;
    uint256[3] public slots;

    struct Record {
        uint256 id;
        uint256 value;
        bool active;
    }
    Record public record;

    event Done();

    function run() public returns (uint256) {
        balances[msg.sender] = 100;

        slots[0] = 1;
        slots[1] = 2;
        slots[2] = 3;

        record = Record({id: 7, value: 42, active: true});

        emit Done();
        return slots[0] + slots[1] + slots[2] + record.value; // 1+2+3+42 = 48
    }
}
