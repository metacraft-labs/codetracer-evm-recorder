// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title CustomErrors — exercise Solidity 0.8.4+ typed custom errors.
///
/// Two custom errors with different signatures:
///   * `Unauthorized()`         — selector-only, no payload.
///   * `InsufficientBalance(uint256 available, uint256 required)` —
///                                two-argument typed payload.
///
/// `withdraw(amount)` triggers `Unauthorized` for non-owner callers
/// and `InsufficientBalance` when the requested amount exceeds the
/// stored balance.  The recorder must surface each `revert` as an
/// `EventLogKind::Error` io_event whose decoded text spells out the
/// error name (and ABI-decoded arguments where applicable) instead of
/// dumping a raw 4-byte selector hex blob.
///
/// `triggerUnauthorized()` and `triggerInsufficient()` are the
/// parameterless test entry points the recorder CLI invokes.  Both
/// are guaranteed to revert at the dispatcher's first user-code
/// statement; the test asserts each ioError text matches the canonical
/// decoded form.
contract CustomErrors {
    address public owner;
    uint256 public balance;

    error Unauthorized();
    error InsufficientBalance(uint256 available, uint256 required);

    constructor() {
        owner = msg.sender;
        balance = 50;
    }

    function withdraw(uint256 amount) public {
        if (msg.sender != owner) revert Unauthorized();
        if (amount > balance) revert InsufficientBalance(balance, amount);
        balance -= amount;
    }

    /// Hits the `Unauthorized` branch when called from a non-owner
    /// `--from` address.  When called from the owner (default), it
    /// falls through to the balance check, which passes (amount=0
    /// can never exceed balance=50), so the function returns
    /// successfully and the test fails loud.  We pair this with
    /// `--from` set to anvil's `accounts[1]` to drive the revert.
    function triggerUnauthorized() public {
        withdraw(0);
    }

    /// Hits the `InsufficientBalance` branch — called by the owner
    /// with `amount=100` (>50), so the second `revert` fires and
    /// surfaces both ABI-encoded args (available=50, required=100).
    function triggerInsufficient() public {
        withdraw(100);
    }
}
