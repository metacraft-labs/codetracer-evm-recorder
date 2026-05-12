// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title EventsTest — exercise event emission (Solidity `emit`).
///
/// `run()` emits three distinct events with progressively more
/// indexed/non-indexed parameters so the recorder must surface a
/// `RecordEvent` (mapped to `EventLogKind::EvmEvent`) for each
/// `LOG{n}` opcode the EVM executes.  The variant differences also
/// guarantee that a recorder which deduplicates by topic-hash would
/// fail the count assertion in the test.
contract EventsTest {
    uint256 public stored;

    event Started();                        // LOG1 (anonymous topic = signature)
    event Tagged(uint256 indexed key);      // LOG2
    event Payload(uint256 indexed key, uint256 value); // LOG2 + data

    function run() public returns (uint256) {
        emit Started();
        emit Tagged(7);
        emit Payload(7, 42);
        stored = 42;
        return 42;
    }
}
