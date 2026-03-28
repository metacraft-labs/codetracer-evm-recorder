// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract SyntaxTest {
    // --- Internal helpers ---

    function _compute(uint a, uint b) internal pure returns (uint sum, uint product) {
        sum = a + b;
        product = a * b;
    }

    // --- 1. Named return variables ---

    function namedReturns(uint a, uint b) public pure returns (uint sum, uint product) {
        sum = a + b;
        product = a * b;
    }

    // --- 2. For-loop variable ---

    function forLoop(uint n) public pure returns (uint total) {
        total = 0;
        for (uint i = 0; i < n; i++) {
            total += i;
        }
    }

    // --- 3. While loop ---

    function whileLoop(uint n) public pure returns (uint total) {
        total = 0;
        uint counter = 0;
        while (counter < n) {
            total += counter;
            counter++;
        }
    }

    // --- 4. Unchecked arithmetic ---

    function uncheckedMath(uint a, uint b) public pure returns (uint result) {
        unchecked {
            uint temp = a * b;
            result = temp + a - b;
        }
    }

    // --- 5. Compound assignments ---

    function compoundOps(uint x) public pure returns (uint result) {
        result = x;
        result += 10;
        result -= 3;
        result *= 2;
    }

    // --- 6. Increment / decrement ---

    function incrDecr(uint x) public pure returns (uint a, uint b) {
        a = x++;
        b = ++x;
        a = x--;
        b = --x;
    }

    // --- 7. Tuple destructuring ---

    function tupleDestructure(uint a, uint b) public pure returns (uint sum, uint product) {
        (uint s, uint p) = _compute(a, b);
        sum = s;
        product = p;
    }

    // --- 8. Block scoping ---

    function blockScope(uint x) public pure returns (uint result) {
        {
            uint temp = x + 1;
            result = temp;
        }
        {
            uint temp = x * 2;
            result += temp;
        }
    }

    // --- 9. Ternary / conditional ---

    function ternary(bool flag, uint a, uint b) public pure returns (uint result) {
        result = flag ? a : b;
    }

    // --- 10. Type conversions ---

    function typeCast(uint x) public pure returns (address addr, bool flag, bytes32 hash) {
        addr = address(uint160(x));
        flag = x != 0;
        hash = bytes32(x);
    }

    // --- 11. Multiple assignments ---

    function multiAssign(uint a, uint b) public pure returns (uint x) {
        x = a;
        x = x + b;
        x = x * 2;
    }
}
