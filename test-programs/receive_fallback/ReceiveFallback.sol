// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title ReceiveFallback — exercise the `receive()` and `fallback()`
/// special entry points.
///
/// `receive()` is invoked by the EVM when a CALL targets the contract
/// with **empty calldata** (a plain ETH transfer).  `fallback()` is
/// invoked when a CALL targets the contract with **non-empty
/// calldata that doesn't match any function selector**.  Each path
/// increments a separate counter so the strict pin can verify which
/// entry-point was hit.
///
/// Two parameterless dispatcher entry-points the recorder CLI can
/// invoke directly:
///
///   * `triggerReceive()` — does `address(this).call{value: 0}("")` →
///     routes through `receive()`,
///   * `triggerFallback()` — does `address(this).call(hex"deadbeef")` →
///     routes through `fallback()` (selector `0xdeadbeef` is not in
///     the ABI).
///
/// Both inner CALLs land at depth +1 as `external_call_depth_2`
/// frames in the trace; the strict pins assert the correct counter
/// was incremented (`receivedCount` vs `fallbackCount`) and that the
/// inner-frame source line corresponds to the special function's
/// body.
contract ReceiveFallback {
    uint256 public receivedCount;
    uint256 public fallbackCount;
    uint256 public lastValue;
    uint256 public lastDataLen;

    event ReceiveHit(uint256 value, uint256 totalCount);
    event FallbackHit(uint256 dataLen, uint256 totalCount);

    receive() external payable {
        receivedCount += 1;
        lastValue = msg.value;
        emit ReceiveHit(msg.value, receivedCount);
    }

    fallback() external payable {
        fallbackCount += 1;
        lastDataLen = msg.data.length;
        emit FallbackHit(msg.data.length, fallbackCount);
    }

    function triggerReceive() public returns (uint256) {
        (bool ok, ) = address(this).call{value: 0}("");
        require(ok, "receive call failed");
        return receivedCount;
    }

    function triggerFallback() public returns (uint256) {
        (bool ok, ) = address(this).call(hex"deadbeef");
        require(ok, "fallback call failed");
        return fallbackCount;
    }
}
