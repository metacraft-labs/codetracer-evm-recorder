// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/// @title Inheritance — exercise multi-level virtual inheritance + `super`.
///
/// Three-level inheritance chain: `Base` <- `Mid` <- `Leaf`.  Each
/// override of `foo()` calls `super.foo()` and adds its own
/// contribution.  When `Leaf.foo()` runs, the recorder must see three
/// internal call frames in LIFO order:
///
///   Leaf.foo()  → returns super.foo() + 100  =  11 + 100 = 111
///   Mid.foo()   → returns super.foo() +  10  =   1 +  10 =  11
///   Base.foo()  → returns 1
///
/// Because `super` resolves at compile time (solc inlines the
/// dispatcher into the contract code), all three frames execute
/// against the **same** contract address (the deployed `Leaf`).  No
/// DELEGATECALL or CALL opcodes are emitted; only internal JUMPs.
///
/// The fixture pins:
///   * the function table contains all three `foo` overrides under
///     their `<Contract>.foo` AST-resolved names,
///   * the call-entry sequence walks Leaf.foo → Mid.foo → Base.foo,
///   * the final value `111` is emitted as a `Result(uint256)` event.
contract Base {
    function foo() public virtual returns (uint256) {
        return 1;
    }
}

contract Mid is Base {
    function foo() public virtual override returns (uint256) {
        return super.foo() + 10;
    }
}

/// `Inheritance` is the leaf of the chain — picked by the recorder
/// CLI's file-stem-matching heuristic so `run()` lands on it.
contract Inheritance is Mid {
    event Result(uint256 v);

    function foo() public override returns (uint256) {
        return super.foo() + 100;
    }

    function run() public returns (uint256) {
        uint256 v = foo();
        emit Result(v);
        return v;
    }
}
