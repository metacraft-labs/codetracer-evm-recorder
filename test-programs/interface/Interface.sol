// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Interface — exercise interface declarations + cross-contract
/// dispatch through an interface reference.
///
/// Three contracts:
///
///   * `IERC20` — the canonical 6-function ERC-20 interface
///     (`totalSupply`, `balanceOf`, `transfer`, `allowance`,
///     `approve`, `transferFrom`).  Interfaces are pure declarations
///     — they have no body, no storage, no bytecode.
///   * `Token` — a minimal implementing contract that satisfies
///     the IERC20 interface.  Its `transfer` writes to a real
///     balances mapping and emits the canonical `Transfer` event.
///   * `Interface` — the consumer.  It deploys a `Token`, holds it
///     under an `IERC20` reference, and calls `t.transfer(...)` via
///     the interface reference.
///
/// Even though the call site reads as `t.transfer(beef, 7)` (where
/// `t` is typed as `IERC20`), at runtime the dispatch lands on the
/// implementing `Token` contract via a normal external CALL.  The
/// strict pin asserts:
///
///   * the inner CALL surfaces as exactly one `external_call_depth_2`
///     placeholder frame,
///   * inside that frame, the source-line steps land in `Token`'s
///     bytecode (NOT in IERC20's empty-body interface declarations,
///     which produce no bytecode at all),
///   * the canonical `Transfer(from, to, value)` event surfaces as
///     a LOG3 ioStderr io_event with `value=7`.
contract Interface {
    IERC20 public token;

    event Done(uint256 v);

    function run() public returns (uint256) {
        token = new Token();
        bool ok = token.transfer(address(0xBEEF), 7);
        require(ok, "transfer failed");
        uint256 bal = token.balanceOf(address(0xBEEF));
        emit Done(bal);
        return bal;
    }
}

interface IERC20 {
    function totalSupply() external view returns (uint256);
    function balanceOf(address account) external view returns (uint256);
    function transfer(address recipient, uint256 amount) external returns (bool);
    function allowance(address owner, address spender) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transferFrom(address sender, address recipient, uint256 amount) external returns (bool);
}

contract Token is IERC20 {
    mapping(address => uint256) public balances;
    mapping(address => mapping(address => uint256)) public allowances;
    uint256 public override totalSupply;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);

    constructor() {
        balances[msg.sender] = 1000;
        totalSupply = 1000;
    }

    function balanceOf(address account) external view override returns (uint256) {
        return balances[account];
    }

    function transfer(address recipient, uint256 amount) external override returns (bool) {
        balances[msg.sender] -= amount;
        balances[recipient] += amount;
        emit Transfer(msg.sender, recipient, amount);
        return true;
    }

    function allowance(address owner, address spender) external view override returns (uint256) {
        return allowances[owner][spender];
    }

    function approve(address spender, uint256 amount) external override returns (bool) {
        allowances[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transferFrom(address sender, address recipient, uint256 amount) external override returns (bool) {
        allowances[sender][msg.sender] -= amount;
        balances[sender] -= amount;
        balances[recipient] += amount;
        emit Transfer(sender, recipient, amount);
        return true;
    }
}
