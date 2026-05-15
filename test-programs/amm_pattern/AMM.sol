// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title AMM — minimal Uniswap-V2-style constant-product AMM.
///
/// Maintains two reserves `(reserveA, reserveB)` for a single trading
/// pair and exposes `swap(amountIn) → amountOut` honouring the
/// constant-product invariant `reserveA * reserveB = k`.
///
/// `run()` seeds the pool with `(1000, 1000)` and performs a single
/// `swap(100)` (token A in, token B out).  After the swap:
///
///   * `newReserveA = reserveA + amountIn = 1100`
///   * `newReserveB = (reserveA * reserveB) / newReserveA
///                  = (1000 * 1000) / 1100 = 909` (integer divide)
///   * `amountOut   = reserveB - newReserveB = 1000 - 909 = 91`
///
/// The strict pin asserts:
///
///   * the `Swap(...)` event encodes `amountIn=100, amountOut=91`
///     (= 0x5b),
///   * both reserve SSTOREs surface (slot 0 = reserveA, slot 1 =
///     reserveB).
contract AMM {
    uint256 public reserveA;
    uint256 public reserveB;

    event Swap(uint256 amountIn, uint256 amountOut);

    function run() public returns (uint256) {
        // Seed the pool.
        reserveA = 1000;
        reserveB = 1000;
        // Single swap.
        return _swap(100);
    }

    function _swap(uint256 amountIn) internal returns (uint256) {
        uint256 k = reserveA * reserveB;
        uint256 newReserveA = reserveA + amountIn;
        uint256 newReserveB = k / newReserveA;
        uint256 amountOut = reserveB - newReserveB;
        reserveA = newReserveA;
        reserveB = newReserveB;
        emit Swap(amountIn, amountOut);
        return amountOut;
    }
}
