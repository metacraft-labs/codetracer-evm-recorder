// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title ERC20 — minimal ERC-20-shaped fixture exercising the canonical
///                token storage / event / dispatch shape.
///
/// `run()` is the parameterless entry point (per the recorder CLI
/// convention) and performs a self-contained mint + transfer dance:
///
///   1. Mint `1000` tokens to `address(this)` (the contract is its
///      own initial holder, so the test can fully drive token flow
///      without external accounts).
///   2. Approve `address(0xBEEF)` for `300` tokens.
///   3. Transfer `250` tokens to `address(0xCAFE)` via the internal
///      `_transfer` helper (the same helper public `transfer` would
///      hit, so this exercises the canonical mapping read / write +
///      `Transfer` event emission).
///
/// The fixture pins all six canonical ERC-20 functions
/// (`totalSupply`, `balanceOf`, `transfer`, `approve`, `transferFrom`,
/// `allowance`) so the function table contains the canonical names,
/// even when `run()` only invokes a subset internally.  The
/// non-invoked ones are still compiled into the runtime bytecode and
/// surface in the AST.
contract ERC20 {
    string public name = "TestToken";
    string public symbol = "TT";
    uint8 public decimals = 18;

    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);

    function run() public returns (uint256) {
        // --- mint 1000 to self ---
        totalSupply = totalSupply + 1000;
        balanceOf[address(this)] = balanceOf[address(this)] + 1000;
        emit Transfer(address(0), address(this), 1000);

        // --- approve 300 to 0xBEEF ---
        allowance[address(this)][address(0xBEEF)] = 300;
        emit Approval(address(this), address(0xBEEF), 300);

        // --- transfer 250 to 0xCAFE ---
        uint256 sent = _transfer(address(this), address(0xCAFE), 250);

        return balanceOf[address(this)] + sent;
    }

    function _transfer(address from, address to, uint256 value)
        internal
        returns (uint256)
    {
        balanceOf[from] = balanceOf[from] - value;
        balanceOf[to] = balanceOf[to] + value;
        emit Transfer(from, to, value);
        return value;
    }

    function transfer(address to, uint256 value) public returns (bool) {
        _transfer(msg.sender, to, value);
        return true;
    }

    function approve(address spender, uint256 value) public returns (bool) {
        allowance[msg.sender][spender] = value;
        emit Approval(msg.sender, spender, value);
        return true;
    }

    function transferFrom(address from, address to, uint256 value)
        public
        returns (bool)
    {
        allowance[from][msg.sender] = allowance[from][msg.sender] - value;
        _transfer(from, to, value);
        return true;
    }
}
