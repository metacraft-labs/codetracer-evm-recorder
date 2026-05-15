// SPDX-License-Identifier: MIT

// PureYul -- standalone Yul object compiled via solc --strict-assembly.
//
// Yul objects have a different shape from Solidity contracts:
//   * the outer `object "Name"` carries the constructor `code { ... }`
//     (this is what gets deployed),
//   * the inner `object "runtime"` carries the runtime `code { ... }`
//     (this is what executes when the contract is called).
//
// The constructor copies the runtime to memory and RETURNs it; from
// then on the runtime object's `code` block executes for every call
// regardless of calldata.  There's no ABI dispatcher and no selector,
// so the recorder calls with empty calldata.
//
// The runtime computes `15 + 27 = 42` via a Yul function, stores the
// result to slot 0, MSTOREs it into memory, and RETURNs the 32-byte
// big-endian encoding (so the on-chain return value pins the result).
object "PureYul" {
  code {
    let runtimeSize := datasize("runtime")
    let runtimeOffset := dataoffset("runtime")
    codecopy(0, runtimeOffset, runtimeSize)
    return(0, runtimeSize)
  }
  object "runtime" {
    code {
      function computeAdd(a, b) -> r {
        r := add(a, b)
      }
      let result := computeAdd(15, 27)
      sstore(0, result)
      mstore(0, result)
      return(0, 32)
    }
  }
}
