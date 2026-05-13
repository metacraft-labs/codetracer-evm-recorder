// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title IndexedEvents — exercise every LOG{0..4} arity with mixed
///                       indexed / non-indexed parameters.
///
/// `run()` emits four event shapes back-to-back:
///
///   - `Anon()`                                                 → LOG1
///       (Solidity always emits a topic0 = signature hash unless
///        the event is declared `anonymous`; we don't declare it
///        anonymous here so the encoder produces LOG1.)
///   - `Single(uint256 indexed a)`                              → LOG2
///   - `Pair(address indexed from, address indexed to,
///           uint256 amount)`                                   → LOG3
///       (the canonical ERC-20 `Transfer` shape: two indexed
///        addresses + one non-indexed uint256 data word.)
///   - `Quad(uint256 indexed a, uint256 indexed b,
///           uint256 indexed c, bytes data)`                    → LOG4
///       (a four-topic event with a *dynamic* `bytes data` payload —
///        the data segment carries the ABI head [offset, length]
///        + the dynamic body.)
///
/// This is the **M9-deferred LOG{n} ABI-decoding pin**: the recorder
/// must surface each emission as an `EvmEvent` io_event with the
/// indexed args in the topics list AND the non-indexed args decoded
/// out of the `data` (memory) segment per ABI.
contract IndexedEvents {
    event Anon();
    event Single(uint256 indexed a);
    event Pair(address indexed from, address indexed to, uint256 amount);
    event Quad(
        uint256 indexed a,
        uint256 indexed b,
        uint256 indexed c,
        bytes data
    );

    function run() public returns (uint256) {
        emit Anon();
        emit Single(11);
        emit Pair(address(0xAA), address(0xBB), 222);
        emit Quad(1, 2, 3, hex"deadbeef");
        return 1;
    }
}
