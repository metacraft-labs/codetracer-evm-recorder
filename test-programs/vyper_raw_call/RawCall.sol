// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title RawCall — Solidity substitute for the Vyper `raw_call(...)`
/// fixture.
///
/// Vyper is not available in the dev shell, so the
/// `vyper_raw_call_test` slot is filled by a Solidity fixture
/// exercising the analogous low-level `(bool ok, bytes memory ret) =
/// target.call(payload)` primitive.
///
/// Solidity's low-level `address.call(bytes)` is the closest analogue
/// of Vyper's `raw_call(target, data)` — it lowers to a CALL opcode
/// with arbitrary calldata and propagates back the (success, return-
/// data) tuple.  The strict pin asserts:
///
///   * the recorder surfaces exactly one EXTERNAL CALL placeholder
///     frame (the `target.call(payload)` invocation),
///   * the `Result(uint256)` event encodes the value returned by the
///     callee (`Target.echo(7) = 7 * 2 + 1 = 15 = 0xf`),
///   * the function table contains the entry-point `run`.
contract Target {
    function echo(uint256 v) external pure returns (uint256) {
        return v * 2 + 1;
    }
}

contract RawCall {
    event Result(uint256 v);

    function run() public returns (uint256) {
        Target t = new Target();
        // Build the raw payload `echo(uint256)` selector + abi.encode(7)
        // by hand so the call site exercises the low-level
        // `address.call(bytes)` primitive (NOT the high-level
        // `t.echo(7)` typed call).
        bytes memory payload = abi.encodeWithSelector(Target.echo.selector, uint256(7));
        (bool ok, bytes memory retdata) = address(t).call(payload);
        require(ok, "raw call failed");
        uint256 v = abi.decode(retdata, (uint256));
        emit Result(v);
        return v;
    }
}
