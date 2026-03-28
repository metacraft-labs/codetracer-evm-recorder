// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract FlowTest {
    uint256 public storedA;
    uint256 public storedResult;

    event Computed(uint256 result);

    function compute() public returns (uint256) {
        uint256 a = 10;
        uint256 b = 20;
        storedA = a;
        uint256 result = add(a, b);
        storedResult = result;
        emit Computed(result);
        return result;
    }

    function add(uint256 x, uint256 y) internal pure returns (uint256) {
        return x + y;
    }
}
