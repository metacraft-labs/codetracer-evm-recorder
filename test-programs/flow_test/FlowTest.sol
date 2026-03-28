// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract FlowTest {
    uint256 public storedA;
    uint256 public storedResult;

    function compute() public returns (uint256) {
        uint256 a = 10;
        uint256 b = 32;
        uint256 sum_val = a + b;      // 42
        uint256 doubled = sum_val * 2; // 84
        uint256 final_result = doubled + a; // 94
        storedA = a;
        storedResult = final_result;
        return final_result;
    }
}
