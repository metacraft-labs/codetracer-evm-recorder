// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

//
// Decorators -- Solidity substitute for the Vyper decorator fixture.
//
// Vyper is not available in the dev shell.  Vyper decorators
// (external / internal / view / pure / payable / nonpayable) map
// almost 1:1 to Solidity function modifiers (external, internal,
// view, pure, payable, default nonpayable); this fixture exercises
// one function in each decorator combination and a parameterless
// run() entry-point that invokes them.
//
// Note: regular `//` comments are used here (NOT `///` natspec
// docstrings) because solc interprets `@external` / `@payable` /
// `@nonpayable` inside doc comments as natspec tags and rejects them
// as not valid for contracts.
//
// The combinations exercised here are intentionally distinct from
// `visibility_test` (which only covers the four visibility levels):
//
//   * pureView    -- internal pure, returns a constant.
//   * viewState   -- internal view, reads a storage slot.
//   * stateMut    -- internal, writes a storage slot.
//   * payableEntry-- external payable, accepts ETH.
//
// run() calls all four (the payable one through this.payableEntry()
// to drive the dispatcher's CALLVALUE check).  The strict pin asserts
// the sum of returns and the SSTORE round-trip.
//
contract Decorators {
    uint256 public stored;

    event Total(uint256 v);

    function pureView() internal pure returns (uint256) {
        return 100;
    }

    function viewState() internal view returns (uint256) {
        return stored;
    }

    function stateMut(uint256 v) internal {
        stored = v;
    }

    function payableEntry() external payable returns (uint256) {
        return 11;
    }

    function run() public returns (uint256) {
        uint256 a = pureView();
        stateMut(50);
        uint256 b = viewState();
        uint256 c = this.payableEntry();
        uint256 total = a + b + c;
        emit Total(total);
        return total;
    }
}
