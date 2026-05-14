// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title SelfDestruct — exercise the `SELFDESTRUCT` opcode.
///
/// `destroy(address payable beneficiary)` calls
/// `selfdestruct(beneficiary)`.  Solidity-0.8.x compiles this to a
/// single `SELFDESTRUCT` opcode that transfers any remaining ETH
/// balance to `beneficiary` and (under EIP-6780, post-Cancun) wipes
/// the contract's code if it was deployed in the same transaction.
///
/// `run()` is the parameterless entry the recorder CLI invokes.  It
/// emits a `BeforeDestroy(...)` event so the trace surfaces a marker
/// just before the SELFDESTRUCT opcode, then calls
/// `selfdestruct(payable(msg.sender))` to terminate execution.
///
/// The strict pin asserts the recorder surfaces the SELFDESTRUCT
/// opcode as a tagged `EventLogKind::EvmEvent` io_event whose
/// `metadata` is `"SELFDESTRUCT"` and whose `text` carries the
/// 20-byte beneficiary address.  The pre-existing `BeforeDestroy`
/// event surfaces as the usual LOG{n} ioStderr io_event.
contract SelfDestruct {
    event BeforeDestroy(address beneficiary);

    function destroy(address payable beneficiary) public {
        emit BeforeDestroy(beneficiary);
        selfdestruct(beneficiary);
    }

    function run() public {
        destroy(payable(msg.sender));
    }
}
