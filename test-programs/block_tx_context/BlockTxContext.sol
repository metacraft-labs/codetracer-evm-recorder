// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title BlockTxContext — exercise the block/tx context global reads.
///
/// `run()` reads six EVM context globals via dedicated opcodes and
/// emits them as a single `Context(...)` event so the recorder can
/// surface each value in the structured payload:
///
///   * `msg.sender` — `CALLER` opcode (the address that initiated the
///     current call), 20-byte raw.
///   * `msg.value`  — `CALLVALUE` opcode (wei attached to the call),
///     uint256 (here always 0 — the test invokes `run()` without value).
///   * `block.timestamp` — `TIMESTAMP` opcode, uint256 (anvil's block
///     time, varies per run; the test asserts shape, not exact value).
///   * `block.number`    — `NUMBER` opcode, uint256.
///   * `tx.origin`       — `ORIGIN` opcode, 20-byte raw.
///   * `gasleft()`       — `GAS` opcode, uint256.
///
/// The fixture stores the captured tuple into per-field storage slots
/// AND emits a `Context` event so two independent surfaces (the
/// per-step `vars[]` snapshot and the io_event payload) agree on the
/// values.  This makes the strict assertion immune to any single
/// surface drifting.
contract BlockTxContext {
    address public lastSender;
    uint256 public lastValue;
    uint256 public lastTimestamp;
    uint256 public lastNumber;
    address public lastOrigin;
    uint256 public lastGas;

    event Context(
        address sender,
        uint256 value,
        uint256 timestamp,
        uint256 number,
        address origin,
        uint256 gas
    );

    function run() public payable returns (uint256) {
        address sender = msg.sender;
        uint256 value = msg.value;
        uint256 ts = block.timestamp;
        uint256 num = block.number;
        address origin = tx.origin;
        uint256 gas = gasleft();

        lastSender = sender;
        lastValue = value;
        lastTimestamp = ts;
        lastNumber = num;
        lastOrigin = origin;
        lastGas = gas;

        emit Context(sender, value, ts, num, origin, gas);
        return num;
    }
}
