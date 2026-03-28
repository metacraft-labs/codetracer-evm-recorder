// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title MappingTest — Exercises mappings, arrays, structs, and storage patterns.
///
/// Designed to test SSTORE/SLOAD tracing for:
///   - Simple mapping (address => uint256)
///   - Nested mapping (address => (uint256 => uint256))
///   - Fixed-size storage array
///   - Struct with multiple fields
contract MappingTest {
    // --- Simple mapping ---
    mapping(address => uint256) public balances;

    // --- Nested mapping ---
    mapping(address => mapping(uint256 => uint256)) public allowances;

    // --- Fixed-size storage array ---
    uint256[3] public slots;

    // --- Struct with multiple fields ---
    struct Record {
        uint256 id;
        uint256 value;
        bool active;
    }
    Record public record;

    // --- Counter for total operations ---
    uint256 public opCount;

    event BalanceSet(address indexed user, uint256 amount);
    event AllowanceSet(address indexed user, uint256 key, uint256 value);
    event SlotsUpdated(uint256 s0, uint256 s1, uint256 s2);
    event RecordUpdated(uint256 id, uint256 value, bool active);

    /// Set a balance in the simple mapping.
    /// Exercises: SSTORE via mapping hash.
    function setBalance(address user, uint256 amount) public {
        balances[user] = amount;
        opCount += 1;
        emit BalanceSet(user, amount);
    }

    /// Set an allowance in the nested mapping.
    /// Exercises: SSTORE via double-hash (nested mapping).
    function setAllowance(address user, uint256 key, uint256 value) public {
        allowances[user][key] = value;
        opCount += 1;
        emit AllowanceSet(user, key, value);
    }

    /// Fill the fixed-size array with values.
    /// Exercises: SSTORE at sequential storage slots.
    function fillSlots(uint256 a, uint256 b, uint256 c) public {
        slots[0] = a;
        slots[1] = b;
        slots[2] = c;
        opCount += 1;
        emit SlotsUpdated(a, b, c);
    }

    /// Update the struct.
    /// Exercises: SSTORE for struct fields at packed/sequential slots.
    function setRecord(uint256 id, uint256 value, bool active) public {
        record = Record(id, value, active);
        opCount += 1;
        emit RecordUpdated(id, value, active);
    }

    /// Perform all operations in a single transaction.
    /// Useful for testing multiple SSTORE patterns in one trace.
    function doAll(
        address user,
        uint256 balance,
        uint256 allowanceKey,
        uint256 allowanceVal,
        uint256 s0,
        uint256 s1,
        uint256 s2,
        uint256 recordId,
        uint256 recordVal,
        bool recordActive
    ) public {
        setBalance(user, balance);
        setAllowance(user, allowanceKey, allowanceVal);
        fillSlots(s0, s1, s2);
        setRecord(recordId, recordVal, recordActive);
    }
}
