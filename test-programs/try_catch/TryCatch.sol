// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title TryCatch — exercise Solidity's structured try/catch shape.
///
/// `run()` is the parameterless entry-point.  It deploys a tiny
/// `Callee` (via the in-tx `new` keyword → CREATE) and then invokes
/// `Callee.fn(...)` three times via `try ... catch ...` blocks:
///
///   - `Callee.ok()`        succeeds → catch arms are skipped, the
///                          return value `1` lands in the
///                          `try` clause's binding (`uint256 v`).
///   - `Callee.failStr()`   reverts with `require(false, "boom")` —
///                          the `catch Error(string memory reason)`
///                          arm fires; `reason == "boom"`.
///   - `Callee.failPanic()` triggers `Panic(uint256)` (here via a
///                          division by zero, panic code = 0x12) —
///                          the `catch Panic(uint256 c)` arm fires;
///                          `c == 0x12`.
///
/// All three paths must produce *balanced* Call/Return pairs (the
/// reverts are *caught*, not propagated up to the recorder's
/// top-level revert handler).  A spec-compliant trace would also
/// surface the `reason` / `c` catch-clause parameters as typed
/// `ValueRecord` variables, and the second + third invocations as
/// caught-error io_events.  The spec-correct shape lives in the
/// `_catches_emit_error_events` ignored sibling — it gates on the
/// recorder learning to recognise the inner-CALL revert pattern,
/// which is the structural follow-up to the M9-deferred
/// top-level revert capture.
contract Callee {
    function ok() public pure returns (uint256) {
        return 1;
    }

    function failStr() public pure returns (uint256) {
        require(false, "boom");
        return 0; // unreachable
    }

    function failPanic() public pure returns (uint256) {
        uint256 x = 0;
        // Division by zero → Panic(uint256) with code 0x12.
        return 1 / x;
    }
}

contract TryCatch {
    uint256 public okValue;
    string public lastReason;
    uint256 public lastPanic;

    event Outcome(uint256 ok, uint256 panic);

    function run() public returns (uint256) {
        Callee c = new Callee();

        // --- happy path ---
        try c.ok() returns (uint256 v) {
            okValue = v;
        } catch {
            okValue = 0;
        }

        // --- caught Error(string) ---
        try c.failStr() returns (uint256) {
            // unreachable
        } catch Error(string memory reason) {
            lastReason = reason;
        } catch {
            lastReason = "other";
        }

        // --- caught Panic(uint256) ---
        try c.failPanic() returns (uint256) {
            // unreachable
        } catch Panic(uint256 code) {
            lastPanic = code;
        } catch {
            lastPanic = 0;
        }

        emit Outcome(okValue, lastPanic);
        return okValue + lastPanic;
    }
}
