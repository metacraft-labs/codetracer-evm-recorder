//! Symbolic stack tracker for local variable reconstruction.
//!
//! Solidity (unoptimized) stores local variables at fixed stack positions
//! within their lexical scope.  This module maintains a parallel "symbolic"
//! stack where each slot may carry a variable name.  By tracking the effect
//! of every EVM opcode on the stack we can, at any step, know which stack
//! slot holds which variable and read the concrete value from the structLog's
//! `stack` field.
//!
//! **Limitations (by design)**
//! - Intended for *unoptimized* Solidity code only.  The optimizer can
//!   completely rearrange or eliminate stack slots.
//! - We do not model memory-based variables (structs, dynamic arrays, etc.).
//! - Complex control flow (loops, nested calls) may cause slot assignments to
//!   drift.  The tracker resets on function calls/returns.

use alloy::primitives::U256;
use crate::solidity_ast::VarDecl;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A detected assignment of a concrete stack slot to a variable name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarAssignment {
    /// The variable name that was assigned to a stack slot.
    pub name: String,
    /// Index into the stack (bottom = 0, top = len-1), same convention as
    /// structLog's `stack` array.
    pub stack_position: usize,
}

/// Symbolic stack tracker.
///
/// The `slots` vector mirrors the EVM stack bottom-to-top, matching the
/// ordering of structLog's `stack` array.  Each element is either `None`
/// (anonymous / compiler-generated value) or `Some(name)` (a named variable).
#[derive(Debug, Default)]
pub struct StackTracker {
    /// Symbolic labels, bottom-to-top (index 0 = stack bottom).
    slots: Vec<Option<String>>,
}

impl StackTracker {
    /// Create a new, empty tracker.
    pub fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// Reset to empty state (e.g. on function entry or return).
    pub fn reset(&mut self) {
        self.slots.clear();
    }

    /// Return the current depth (number of items on the symbolic stack).
    pub fn depth(&self) -> usize {
        self.slots.len()
    }

    /// Process one EVM opcode step, updating the symbolic stack and returning
    /// any newly detected variable assignments.
    ///
    /// # Arguments
    ///
    /// * `opcode` — the raw opcode byte (e.g. `0x01` for ADD).
    /// * `pc` — program counter of this instruction (currently unused but
    ///   available for future heuristics).
    /// * `source_offset` — byte offset in the source file for the instruction
    ///   that *produced* the value being pushed (used to match variable
    ///   declarations).
    /// * `vars_in_scope` — variables that are in scope at the current source
    ///   position; used to label newly pushed stack slots.
    #[allow(unused_variables)]
    pub fn process_step(
        &mut self,
        opcode: u8,
        pc: usize,
        source_offset: Option<i32>,
        vars_in_scope: &[&VarDecl],
    ) -> Vec<VarAssignment> {
        let mut assignments = Vec::new();

        match opcode {
            // STOP
            0x00 => {}

            // ADD, MUL, SUB, DIV, SDIV, MOD, SMOD, EXP, SIGNEXTEND (pop 2, push 1)
            0x01..=0x07 | 0x0a | 0x0b => { self.pop_n(2); self.push_anon(); }
            // ADDMOD, MULMOD (pop 3, push 1)
            0x08 | 0x09 => { self.pop_n(3); self.push_anon(); }

            // LT, GT, SLT, SGT, EQ
            0x10..=0x14 => { self.pop_n(2); self.push_anon(); }
            // ISZERO
            0x15 => { self.pop_n(1); self.push_anon(); }
            // AND, OR, XOR
            0x16..=0x18 => { self.pop_n(2); self.push_anon(); }
            // NOT
            0x19 => { self.pop_n(1); self.push_anon(); }
            // BYTE
            0x1a => { self.pop_n(2); self.push_anon(); }
            // SHL, SHR, SAR
            0x1b..=0x1d => { self.pop_n(2); self.push_anon(); }

            // SHA3 / KECCAK256
            0x20 => { self.pop_n(2); self.push_anon(); }

            // ADDRESS(0x30): push 1
            0x30 => { self.push_anon(); }
            // BALANCE(0x31): pop 1, push 1
            0x31 => { self.pop_n(1); self.push_anon(); }
            // ORIGIN(0x32), CALLER(0x33), CALLVALUE(0x34): push 1
            0x32..=0x34 => { self.push_anon(); }
            // CALLDATALOAD(0x35): pop 1, push 1
            0x35 => { self.pop_n(1); self.push_anon(); }
            // CALLDATASIZE(0x36), CODESIZE(0x38): push 1
            0x36 | 0x38 => { self.push_anon(); }
            // CALLDATACOPY(0x37), CODECOPY(0x39): pop 3
            0x37 | 0x39 => { self.pop_n(3); }
            // GASPRICE(0x3a): push 1
            0x3a => { self.push_anon(); }
            // EXTCODESIZE(0x3b): pop 1, push 1
            0x3b => { self.pop_n(1); self.push_anon(); }
            // EXTCODECOPY(0x3c): pop 4
            0x3c => { self.pop_n(4); }
            // RETURNDATASIZE(0x3d): push 1
            0x3d => { self.push_anon(); }
            // RETURNDATACOPY(0x3e): pop 3
            0x3e => { self.pop_n(3); }
            // EXTCODEHASH(0x3f): pop 1, push 1
            0x3f => { self.pop_n(1); self.push_anon(); }

            // BLOCKHASH(0x40): pop 1, push 1
            0x40 => { self.pop_n(1); self.push_anon(); }
            // COINBASE(0x41), TIMESTAMP(0x42), NUMBER(0x43), PREVRANDAO(0x44),
            // GASLIMIT(0x45), CHAINID(0x46), SELFBALANCE(0x47), BASEFEE(0x48),
            // BLOBBASEFEE(0x4a): push 1
            0x41..=0x48 | 0x4a => { self.push_anon(); }
            // BLOBHASH(0x49): pop 1 push 1
            0x49 => { self.pop_n(1); self.push_anon(); }

            // POP
            0x50 => { self.pop_n(1); }

            // MLOAD
            0x51 => { self.pop_n(1); self.push_anon(); }
            // MSTORE, MSTORE8
            0x52 | 0x53 => { self.pop_n(2); }

            // SLOAD
            0x54 => { self.pop_n(1); self.push_anon(); }
            // SSTORE
            0x55 => { self.pop_n(2); }

            // JUMP
            0x56 => { self.pop_n(1); }
            // JUMPI
            0x57 => { self.pop_n(2); }
            // PC, MSIZE, GAS
            0x58..=0x5a => { self.push_anon(); }
            // JUMPDEST
            0x5b => {}
            // TLOAD
            0x5c => { self.pop_n(1); self.push_anon(); }
            // TSTORE
            0x5d => { self.pop_n(2); }
            // MCOPY(0x5e): pop 3 (dst, src, length)
            0x5e => { self.pop_n(3); }

            // PUSH0
            0x5f => {
                let label = self.label_for_push(source_offset, vars_in_scope);
                let slot = self.slots.len();
                self.slots.push(label.clone());
                if let Some(name) = label {
                    assignments.push(VarAssignment { name, stack_position: slot });
                }
            }

            // PUSH1..PUSH32 (0x60..0x7f)
            0x60..=0x7f => {
                let label = self.label_for_push(source_offset, vars_in_scope);
                let slot = self.slots.len();
                self.slots.push(label.clone());
                if let Some(name) = label {
                    assignments.push(VarAssignment { name, stack_position: slot });
                }
            }

            // DUP1..DUP16 (0x80..0x8f)
            0x80..=0x8f => {
                let n = (opcode - 0x80 + 1) as usize; // DUP1 duplicates top (position 1)
                let label = self.peek_from_top(n);
                self.slots.push(label);
            }

            // SWAP1..SWAP16 (0x90..0x9f)
            0x90..=0x9f => {
                let n = (opcode - 0x90 + 1) as usize; // SWAP1 swaps top with 2nd
                let len = self.slots.len();
                if len > n {
                    self.slots.swap(len - 1, len - 1 - n);
                }
            }

            // LOG0..LOG4 (0xa0..0xa4)
            0xa0..=0xa4 => {
                let topics = (opcode - 0xa0) as usize;
                self.pop_n(2 + topics);
            }

            // CREATE
            0xf0 => { self.pop_n(3); self.push_anon(); }
            // CALL, CALLCODE
            0xf1 | 0xf2 => { self.pop_n(7); self.push_anon(); }
            // RETURN, REVERT
            0xf3 | 0xfd => { self.pop_n(2); }
            // DELEGATECALL, STATICCALL
            0xf4 | 0xfa => { self.pop_n(6); self.push_anon(); }
            // CREATE2
            0xf5 => { self.pop_n(4); self.push_anon(); }
            // SELFDESTRUCT
            0xff => { self.pop_n(1); }

            // INVALID and unknown opcodes — no stack effect modelled.
            _ => {}
        }

        assignments
    }

    /// Retrieve the current stack value of a named variable from the
    /// *concrete* stack captured in a structLog entry.
    ///
    /// Returns `None` if the variable has no known stack slot or if the
    /// concrete stack is too short.
    pub fn get_variable_value(&self, name: &str, actual_stack: &[U256]) -> Option<U256> {
        // Find the topmost slot labelled with `name` (most-recently-assigned).
        let pos = self.slots.iter().rposition(|s| s.as_deref() == Some(name))?;
        actual_stack.get(pos).copied()
    }

    /// Return all current (name → concrete value) pairs visible from `actual_stack`.
    pub fn all_variable_values(&self, actual_stack: &[U256]) -> Vec<(String, U256)> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();

        // Walk top-to-bottom so the first occurrence of each name is the
        // most-recently pushed / active one.
        for (pos, slot) in self.slots.iter().enumerate().rev() {
            if let Some(name) = slot
                && seen.insert(name.clone())
                && let Some(&val) = actual_stack.get(pos)
            {
                result.push((name.clone(), val));
            }
        }
        result
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    fn pop_n(&mut self, n: usize) {
        let new_len = self.slots.len().saturating_sub(n);
        self.slots.truncate(new_len);
    }

    fn push_anon(&mut self) {
        self.slots.push(None);
    }

    /// Peek at the label `n` positions from the top (1 = top).
    fn peek_from_top(&self, n: usize) -> Option<String> {
        let len = self.slots.len();
        if n == 0 || n > len {
            return None;
        }
        self.slots[len - n].clone()
    }

    /// Choose a label for a freshly-pushed value.
    ///
    /// Strategy: if `source_offset` falls exactly on one of the variables'
    /// declaration offsets, label the new slot with that variable's name.
    fn label_for_push(
        &self,
        source_offset: Option<i32>,
        vars_in_scope: &[&VarDecl],
    ) -> Option<String> {
        let offset = source_offset?;
        // We look for variables whose declaration begins at exactly this offset
        // or within a small window (handles cases where the PUSH is emitted
        // for the RHS of `uint256 a = 10;` which maps to the whole statement).
        for v in vars_in_scope {
            if v.declaration_offset == offset {
                return Some(v.name.clone());
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    fn u256(n: u64) -> U256 {
        U256::from(n)
    }

    #[test]
    fn test_push_pop_depth() {
        let mut t = StackTracker::new();
        assert_eq!(t.depth(), 0);

        // PUSH1
        t.process_step(0x60, 0, None, &[]);
        assert_eq!(t.depth(), 1);

        // Another PUSH1
        t.process_step(0x60, 2, None, &[]);
        assert_eq!(t.depth(), 2);

        // POP
        t.process_step(0x50, 4, None, &[]);
        assert_eq!(t.depth(), 1);

        // ADD (pop 2, push 1)
        t.process_step(0x60, 5, None, &[]); // push to get 2 items
        t.process_step(0x01, 7, None, &[]); // ADD
        assert_eq!(t.depth(), 1);
    }

    #[test]
    fn test_dup_propagates_label() {
        let mut t = StackTracker::new();
        // Declare a dummy var
        let var = VarDecl {
            name: "x".to_string(),
            type_name: "uint256".to_string(),
            src: crate::solidity_ast::SourceRange { offset: 10, length: 9, file_index: 0 },
            declaration_offset: 10,
        };
        let vars = vec![&var];

        // PUSH1 at source offset 10 → should label the slot "x"
        let assignments = t.process_step(0x60, 0, Some(10), &vars);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].name, "x");
        assert_eq!(assignments[0].stack_position, 0);

        // DUP1 → should copy the label
        t.process_step(0x80, 2, None, &[]);
        assert_eq!(t.depth(), 2);

        let stack = vec![u256(42), u256(42)];
        let val = t.get_variable_value("x", &stack);
        assert_eq!(val, Some(u256(42)));
    }

    #[test]
    fn test_swap_rearranges_labels() {
        let mut t = StackTracker::new();

        let var_a = VarDecl {
            name: "a".to_string(),
            type_name: "uint256".to_string(),
            src: crate::solidity_ast::SourceRange { offset: 5, length: 1, file_index: 0 },
            declaration_offset: 5,
        };
        let var_b = VarDecl {
            name: "b".to_string(),
            type_name: "uint256".to_string(),
            src: crate::solidity_ast::SourceRange { offset: 20, length: 1, file_index: 0 },
            declaration_offset: 20,
        };

        // Push a (labelled), then b (labelled)
        t.process_step(0x60, 0, Some(5), &[&var_a]);
        t.process_step(0x60, 2, Some(20), &[&var_b]);

        // Slots: [a, b] (bottom to top)
        // SWAP1 swaps top (b) with second (a) → [b, a]
        t.process_step(0x90, 4, None, &[]);

        // Now top slot is "a" (index 1), bottom is "b" (index 0)
        let stack = vec![u256(20), u256(10)]; // bottom=20, top=10
        assert_eq!(t.get_variable_value("b", &stack), Some(u256(20)));
        assert_eq!(t.get_variable_value("a", &stack), Some(u256(10)));
    }

    #[test]
    fn test_no_assignment_without_matching_offset() {
        let mut t = StackTracker::new();
        let var = VarDecl {
            name: "z".to_string(),
            type_name: "uint256".to_string(),
            src: crate::solidity_ast::SourceRange { offset: 99, length: 5, file_index: 0 },
            declaration_offset: 99,
        };
        // Push at a different offset — should not label
        let assignments = t.process_step(0x60, 0, Some(50), &[&var]);
        assert!(assignments.is_empty());
        assert_eq!(t.depth(), 1);
    }

    #[test]
    fn test_all_variable_values() {
        let mut t = StackTracker::new();
        let var_a = VarDecl {
            name: "a".to_string(),
            type_name: "uint256".to_string(),
            src: crate::solidity_ast::SourceRange { offset: 0, length: 1, file_index: 0 },
            declaration_offset: 0,
        };
        let var_b = VarDecl {
            name: "b".to_string(),
            type_name: "uint256".to_string(),
            src: crate::solidity_ast::SourceRange { offset: 10, length: 1, file_index: 0 },
            declaration_offset: 10,
        };

        t.process_step(0x60, 0, Some(0), &[&var_a]);
        t.process_step(0x60, 2, Some(10), &[&var_b]);

        let stack = vec![u256(10), u256(20)];
        let mut vals = t.all_variable_values(&stack);
        vals.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(vals.len(), 2);
        assert_eq!(vals[0], ("a".to_string(), u256(10)));
        assert_eq!(vals[1], ("b".to_string(), u256(20)));
    }

    #[test]
    fn test_addmod_mulmod_pop3_push1() {
        // ADDMOD (0x08) and MULMOD (0x09) take 3 operands (pop 3, push 1),
        // unlike ADD/MUL which take 2.
        let mut t = StackTracker::new();
        // Push 3 items
        t.process_step(0x60, 0, None, &[]); // depth 1
        t.process_step(0x60, 2, None, &[]); // depth 2
        t.process_step(0x60, 4, None, &[]); // depth 3

        // ADDMOD: pop 3, push 1 → depth stays 1
        t.process_step(0x08, 6, None, &[]);
        assert_eq!(t.depth(), 1, "ADDMOD should pop 3 and push 1");

        // Reset and repeat for MULMOD
        t.reset();
        t.process_step(0x60, 0, None, &[]);
        t.process_step(0x60, 2, None, &[]);
        t.process_step(0x60, 4, None, &[]);
        t.process_step(0x09, 6, None, &[]);
        assert_eq!(t.depth(), 1, "MULMOD should pop 3 and push 1");
    }

    #[test]
    fn test_mcopy_pops_three() {
        // MCOPY (0x5e) takes dst, src, length — pops 3
        let mut t = StackTracker::new();
        t.process_step(0x60, 0, None, &[]); // depth 1
        t.process_step(0x60, 2, None, &[]); // depth 2
        t.process_step(0x60, 4, None, &[]); // depth 3
        t.process_step(0x5e, 6, None, &[]);
        assert_eq!(t.depth(), 0, "MCOPY should pop 3");
    }
}
