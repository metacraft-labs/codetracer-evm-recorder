// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Implements — Solidity substitute for the Vyper
/// `implements: IFoo` fixture.
///
/// Vyper is not available in the dev shell.  Vyper's
/// `implements: IFoo` directive is the analogue of Solidity's
/// `contract Foo is IFoo` syntax — it declares that the contract
/// satisfies a named interface and the compiler enforces the
/// signature match.
///
/// The strict pin asserts:
///
///   * the implementing contract's `act` function surfaces in the
///     function table (resolved by name, NOT a `fn_at_pc_*`
///     placeholder),
///   * the call sequence visits exactly one `Acted(...)` event with
///     the correct topic0 and ABI-encoded payload (`act(5) → 5 * 3
///     = 15 = 0xf`),
///   * the AST resolver picks up the `act` override even though the
///     interface declaration carries no body.
interface IActor {
    function act(uint256 v) external returns (uint256);
}

contract Implements is IActor {
    event Acted(uint256 input, uint256 output);

    function act(uint256 v) external override returns (uint256) {
        uint256 out = v * 3;
        emit Acted(v, out);
        return out;
    }

    function run() public returns (uint256) {
        // Call the implementing function through `this` so the
        // dispatch goes through the dispatcher's selector lookup
        // (the same path an external IActor reference would take).
        IActor self = IActor(address(this));
        return self.act(5);
    }
}
