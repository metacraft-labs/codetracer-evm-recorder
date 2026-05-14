// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title EcRecover — exercise the `ecrecover` precompile (0x01).
///
/// Solidity's built-in `ecrecover(hash, v, r, s)` function calls the
/// EVM precompile at address `0x0000000000000000000000000000000000000001`
/// to recover the signer address from an ECDSA signature.  The
/// precompile is a STATICCALL to address 0x01 with the canonical
/// 128-byte input layout `(hash, v, r, s)`.
///
/// `run()` is the parameterless entry the recorder CLI invokes.  It
/// pins fixed `(hash, v, r, s)` values and emits a `Recovered(addr)`
/// event carrying the recovered signer.  The fixed inputs come from
/// the canonical Ethereum signed-message test vector — they recover
/// to a deterministic, well-known signer address.
///
/// The strict pin asserts:
///   * the inner STATICCALL surfaces as exactly one
///     `external_call_depth_2` placeholder frame that the recorder
///     tags as the `ecrecover` precompile (see
///     `build_precompile_event_content` in `src/recorder.rs`),
///   * the `Recovered(address)` event surfaces as a LOG1 ioStderr
///     io_event whose data segment carries the recovered 20-byte
///     signer address.
contract EcRecover {
    event Recovered(address signer);

    function run() public returns (address) {
        bytes32 hash = 0x47173285a8d7341e5e972fc677286384f802f8ef42a5ec5f03bbfa254cb01fad;
        uint8 v = 28;
        bytes32 r = 0x99e71a99cb2270b8cac5254f9e99b6210c6c10224a1579cf389ef88b20a1abe9;
        bytes32 s = 0x129ff05af364204442bdb53ab6f18a99ab48acc9326fa689f228040429e3ca66;
        address signer = ecrecover(hash, v, r, s);
        emit Recovered(signer);
        return signer;
    }
}
