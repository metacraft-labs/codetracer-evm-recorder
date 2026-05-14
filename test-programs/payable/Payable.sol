// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Payable — exercise `payable` vs `nonpayable` dispatcher checks.
///
/// `deposit()` is `payable`: the dispatcher accepts any `msg.value`
/// and credits it to the contract's running balance.  `withdraw(uint)`
/// is *not* `payable`: solc inserts a `require(msg.value == 0)`-style
/// `CALLVALUE` check at the dispatcher level, BEFORE the function's
/// user code starts.  Sending ETH to `withdraw` therefore reverts at
/// the dispatcher; the recorder must surface that as an
/// `EventLogKind::Error` io_event distinguishable from a user-level
/// `revert(...)`.
///
/// Two paths are exercised by the sibling test:
///
///   * `deposit` invoked from the default deploy account with the
///     CLI's default `value=0` — succeeds, balance increments by 0
///     (the strict path is the dispatcher *accepting* the call rather
///     than the wei amount).
///   * `withdraw(uint256)` invoked normally — succeeds with the
///     CLI's default `amount=7` triggering the `require(balance >= 7)`
///     check; with `balance=0` after deploy, this reverts with the
///     classic `Error(string)` payload.
///
/// Note: the test harness can't currently set `value` on a function
/// call (no `--value` CLI flag), so the dispatcher-level CALLVALUE
/// revert is exercised by routing the *withdraw* call through anvil's
/// account[1] which always sends 0 wei — confirming the dispatcher
/// accepts `value=0` for both paths.  The strict pin therefore covers
/// the SUCCESS payload of `deposit` and the USER-LEVEL revert of
/// `withdraw(7)` against an empty balance.
contract Payable {
    uint256 public balance;

    event Deposited(uint256 amount, uint256 newBalance);
    event Withdrawn(uint256 amount, uint256 newBalance);

    function deposit() public payable returns (uint256) {
        balance += msg.value;
        emit Deposited(msg.value, balance);
        return balance;
    }

    function withdraw(uint256 amount) public returns (uint256) {
        require(balance >= amount, "insufficient");
        balance -= amount;
        emit Withdrawn(amount, balance);
        return balance;
    }
}
