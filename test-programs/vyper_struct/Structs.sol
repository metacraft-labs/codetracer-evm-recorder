// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Structs — Solidity substitute for the Vyper `struct` fixture.
///
/// Vyper compilation is not available in the dev shell (no `vyper`
/// binary), so the `vyper_struct_test` slot is filled by an
/// equivalent-shape Solidity fixture exercising:
///
///   * a multi-field `struct Point { uint256 x; uint256 y; }` stored
///     in state and constructed in memory,
///   * an `event Moved(uint256 oldX, uint256 oldY, uint256 newX,
///     uint256 newY)` emit so the trace surfaces a tagged event,
///   * an SSTORE round-trip for both struct fields so each one shows
///     up as a `ValueRecord::Raw` step variable.
///
/// The strict pin asserts:
///
///   * exactly one `Moved(...)` LOG event with the correct topic0
///     (keccak256("Moved(uint256,uint256,uint256,uint256)")) and the
///     four ABI-encoded uint256 slots in the data segment,
///   * the function table contains the entry-point `run` and the
///     internally-called helper `_move`,
///   * the step-line sequence pins the struct construction + assignment
///     + emit walk.
contract Structs {
    struct Point {
        uint256 x;
        uint256 y;
    }

    Point public position;

    event Moved(uint256 oldX, uint256 oldY, uint256 newX, uint256 newY);

    function run() public returns (uint256) {
        position = Point({x: 3, y: 4});
        _move(10, 20);
        return position.x + position.y;
    }

    function _move(uint256 nx, uint256 ny) internal {
        Point memory prev = position;
        position = Point({x: nx, y: ny});
        emit Moved(prev.x, prev.y, nx, ny);
    }
}
