// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Lending — minimal Compound-style lending contract.
///
/// Maintains per-user deposit + borrow balances on a single underlying
/// asset.  `run()` drives the canonical deposit → borrow → repay →
/// withdraw lifecycle on `address(this)` so the recorder can pin the
/// full state-update sequence without external accounts.
///
/// Concrete numbers (chosen to be small + distinct so each storage
/// SSTORE surfaces a unique value the strict pin can assert):
///
///   1. `_deposit(1000)`  → `deposits[this] = 1000`
///   2. `_borrow(400)`    → `borrows[this]  = 400`
///   3. `_repay(150)`     → `borrows[this]  = 250`
///   4. `_withdraw(300)`  → `deposits[this] = 700`
///
/// Final accounting: deposits=700 (0x2bc), borrows=250 (0xfa).
/// `run()` returns `deposits[this] - borrows[this] = 450 = 0x1c2`.
///
/// The strict pin asserts:
///   * each operation surfaces its own LOG event
///     (`Deposit`, `Borrow`, `Repay`, `Withdraw`),
///   * the final storage values for `deposits[this]` and
///     `borrows[this]` are 0x2bc and 0xfa respectively,
///   * the four internal helpers (`_deposit`, `_borrow`, `_repay`,
///     `_withdraw`) are AST-resolved into the function table.
contract Lending {
    mapping(address => uint256) public deposits;
    mapping(address => uint256) public borrows;

    event Deposit(address indexed user, uint256 amount, uint256 newBalance);
    event Borrow(address indexed user, uint256 amount, uint256 newBalance);
    event Repay(address indexed user, uint256 amount, uint256 newBalance);
    event Withdraw(address indexed user, uint256 amount, uint256 newBalance);

    function run() public returns (uint256) {
        _deposit(1000);
        _borrow(400);
        _repay(150);
        _withdraw(300);
        return deposits[address(this)] - borrows[address(this)];
    }

    function _deposit(uint256 amount) internal returns (uint256) {
        uint256 newBalance = deposits[address(this)] + amount;
        deposits[address(this)] = newBalance;
        emit Deposit(address(this), amount, newBalance);
        return newBalance;
    }

    function _borrow(uint256 amount) internal returns (uint256) {
        uint256 newBalance = borrows[address(this)] + amount;
        borrows[address(this)] = newBalance;
        emit Borrow(address(this), amount, newBalance);
        return newBalance;
    }

    function _repay(uint256 amount) internal returns (uint256) {
        uint256 newBalance = borrows[address(this)] - amount;
        borrows[address(this)] = newBalance;
        emit Repay(address(this), amount, newBalance);
        return newBalance;
    }

    function _withdraw(uint256 amount) internal returns (uint256) {
        uint256 newBalance = deposits[address(this)] - amount;
        deposits[address(this)] = newBalance;
        emit Withdraw(address(this), amount, newBalance);
        return newBalance;
    }

    function deposit(uint256 amount) public returns (uint256) {
        return _deposit(amount);
    }

    function borrow(uint256 amount) public returns (uint256) {
        return _borrow(amount);
    }

    function repay(uint256 amount) public returns (uint256) {
        return _repay(amount);
    }

    function withdraw(uint256 amount) public returns (uint256) {
        return _withdraw(amount);
    }
}
