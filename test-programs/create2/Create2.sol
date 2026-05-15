// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Create2 — exercise CREATE and CREATE2 contract deployment.
///
/// Solidity surfaces two contract-creation forms:
///
///   * `new Child(arg)` — compiles to a `CREATE` opcode whose deployed
///     address depends on the deployer address + nonce.
///   * `new Child{salt: <32-byte>}(arg)` — compiles to a `CREATE2`
///     opcode whose deployed address is fully deterministic — derived
///     from `keccak256(0xff ++ deployer ++ salt ++ keccak256(initCode))[12:]`.
///
/// The fixture deploys two `Child(uint256)` instances from a single
/// factory `run()`:
///
///   1. `c1 = new Child(11)`        — CREATE
///   2. `c2 = new Child{salt: ...}(22)` — CREATE2 with a pinned salt
///
/// Both deployed addresses are surfaced as a `Deployed(c1, c2)` event
/// and stored in storage so the trace pins them.
///
/// The strict pin asserts:
///
///   * exactly two CREATE-shape entries surface as
///     `external_call_depth_2` placeholder frames,
///   * the `Deployed(address,address)` LOG carries both deployed
///     addresses in the data segment,
///   * the CREATE2-deployed address matches the deterministic CREATE2
///     formula (computed off-line from the deployer + salt + initCode
///     hash).
contract Child {
    uint256 public stored;
    constructor(uint256 v) {
        stored = v;
    }
}

contract Create2 {
    address public c1;
    address public c2;

    event Deployed(address create_addr, address create2_addr);

    function run() public {
        Child child1 = new Child(11);
        bytes32 salt = bytes32(uint256(0x123));
        Child child2 = new Child{salt: salt}(22);
        c1 = address(child1);
        c2 = address(child2);
        emit Deployed(c1, c2);
    }
}
