// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Keccak — exercise the `keccak256(...)` builtin.
///
/// `keccak256(bytes)` compiles to the EVM `KECCAK256` (a.k.a. `SHA3`)
/// opcode — it's a native opcode, NOT a precompile call (precompiles
/// 0x01..=0x09 cover ecrecover/sha256/ripemd160/identity/modexp/
/// ecAdd/ecMul/ecPairing/blake2f, but keccak gets its own opcode).
///
/// `run()` computes two deterministic hashes:
///
///   * `h1 = keccak256(abi.encode(uint256(42)))` — a single 32-byte
///     ABI-encoded input.  The output is the canonical pin
///     `0xbeced09521047d05b8960b7e7bcc1d1292cf3e4b2a6b63f48335cbde5f7545d2`.
///   * `h2 = keccak256(abi.encode(uint256(1), uint256(2)))` — two
///     32-byte slots concatenated.  The output is
///     `0xe90b7bceb6e7df5418fb78d8ee546e97c83a08bbccc01a0644d599ccd2a7c2e0`.
///
/// Both hashes are stored to storage AND emitted as a single
/// `Hashed(bytes32, bytes32)` event so the trace surfaces them.
///
/// The strict pin asserts:
///
///   * the function table contains the entry-point `run`,
///   * the `Hashed(...)` LOG{n} event surfaces with both 32-byte
///     hashes packed in the data segment,
///   * the result of each keccak256 call lands in a typed
///     `ValueRecord::Raw` local (`bytes32` round-trips losslessly),
///   * NO precompile-tagged event surfaces (keccak256 is an opcode,
///     not a precompile — the absence is a structural invariant).
contract Keccak {
    bytes32 public h1Storage;
    bytes32 public h2Storage;

    event Hashed(bytes32 h1, bytes32 h2);

    function run() public returns (bytes32, bytes32) {
        bytes32 h1 = keccak256(abi.encode(uint256(42)));
        bytes32 h2 = keccak256(abi.encode(uint256(1), uint256(2)));
        h1Storage = h1;
        h2Storage = h2;
        emit Hashed(h1, h2);
        return (h1, h2);
    }
}
