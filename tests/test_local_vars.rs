//! Tests for M5: Local Variable Reconstruction.
//!
//! These tests do NOT require solc or anvil.  All AST and structLog data is
//! hardcoded so they run in any CI environment.

use alloy::primitives::U256;
use codetracer_evm_recorder::memory_tracker::{
    FREE_MEMORY_POINTER_OFFSET, INITIAL_FREE_MEMORY_POINTER, MemoryTracker,
};
use codetracer_evm_recorder::solidity_ast::{SolidityAst, SourceRange, VarDecl};
use codetracer_evm_recorder::stack_tracker::StackTracker;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn u256(n: u64) -> U256 {
    U256::from(n)
}

fn make_var(name: &str, type_name: &str, offset: i32) -> VarDecl {
    VarDecl {
        name: name.to_string(),
        type_name: type_name.to_string(),
        src: SourceRange {
            offset,
            length: 12,
            file_index: 0,
        },
        declaration_offset: offset,
        statement_range: None,
    }
}

// ---------------------------------------------------------------------------
// test_stack_tracker_unit
// ---------------------------------------------------------------------------

/// Unit test the StackTracker with synthetic opcode sequences.
#[test]
fn test_stack_tracker_unit() {
    let mut t = StackTracker::new();

    // ------- PUSH / POP depth tracking -------
    assert_eq!(t.depth(), 0);
    t.process_step(0x60, 0, None, &[]); // PUSH1 → depth 1
    assert_eq!(t.depth(), 1);
    t.process_step(0x60, 2, None, &[]); // PUSH1 → depth 2
    assert_eq!(t.depth(), 2);
    t.process_step(0x50, 4, None, &[]); // POP   → depth 1
    assert_eq!(t.depth(), 1);

    // ------- ADD (pop 2, push 1) -------
    t.process_step(0x60, 5, None, &[]); // push second item
    assert_eq!(t.depth(), 2);
    t.process_step(0x01, 7, None, &[]); // ADD
    assert_eq!(t.depth(), 1);

    // ------- SSTORE (pop 2) -------
    t.process_step(0x60, 8, None, &[]); // depth = 2
    t.process_step(0x55, 10, None, &[]); // SSTORE
    assert_eq!(t.depth(), 0);
}

/// Verify that DUP propagates the label to the copy.
#[test]
fn test_stack_tracker_dup_label() {
    let mut t = StackTracker::new();
    let var_x = make_var("x", "uint256", 10);

    // Push value labelled "x" (source offset 10 matches var_x.declaration_offset)
    let assignments = t.process_step(0x60, 0, Some(10), &[&var_x]);
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].name, "x");

    // DUP1 — should copy the "x" label
    t.process_step(0x80, 2, None, &[]);
    assert_eq!(t.depth(), 2);

    let concrete = vec![u256(42), u256(42)];
    assert_eq!(t.get_variable_value("x", &concrete), Some(u256(42)));

    // POP the duplicate → "x" still accessible at position 0
    t.process_step(0x50, 3, None, &[]);
    let concrete2 = vec![u256(42)];
    assert_eq!(t.get_variable_value("x", &concrete2), Some(u256(42)));
}

/// Verify that SWAP rearranges labels correctly.
#[test]
fn test_stack_tracker_swap_labels() {
    let mut t = StackTracker::new();
    let var_a = make_var("a", "uint256", 5);
    let var_b = make_var("b", "uint256", 20);

    t.process_step(0x60, 0, Some(5), &[&var_a]); // push "a"
    t.process_step(0x60, 2, Some(20), &[&var_b]); // push "b"
    // Stack: [a(0), b(1)]

    // SWAP1: swaps top (b) with second (a) → [b(0), a(1)]
    t.process_step(0x90, 4, None, &[]);

    let concrete = vec![u256(10), u256(20)];
    // After swap: slot 0 = "b" = 10, slot 1 = "a" = 20
    assert_eq!(t.get_variable_value("b", &concrete), Some(u256(10)));
    assert_eq!(t.get_variable_value("a", &concrete), Some(u256(20)));
}

/// Verify that a PUSH with a non-matching offset produces no assignment.
#[test]
fn test_stack_tracker_no_spurious_label() {
    let mut t = StackTracker::new();
    let var_z = make_var("z", "uint256", 99);

    // Push at offset 50, which does NOT match var_z.declaration_offset (99)
    let assignments = t.process_step(0x60, 0, Some(50), &[&var_z]);
    assert!(
        assignments.is_empty(),
        "should not label a slot when offset doesn't match"
    );
}

/// Verify all_variable_values returns unique names (topmost slot wins).
#[test]
fn test_stack_tracker_all_variable_values() {
    let mut t = StackTracker::new();
    let var_a = make_var("a", "uint256", 0);
    let var_b = make_var("b", "uint256", 10);

    t.process_step(0x60, 0, Some(0), &[&var_a]);
    t.process_step(0x60, 2, Some(10), &[&var_b]);

    let concrete = vec![u256(1), u256(2)];
    let mut vals = t.all_variable_values(&concrete);
    vals.sort_by(|x, y| x.0.cmp(&y.0));

    assert_eq!(vals.len(), 2);
    assert_eq!(vals[0], ("a".to_string(), u256(1)));
    assert_eq!(vals[1], ("b".to_string(), u256(2)));
}

/// If the same variable name appears twice (DUP), all_variable_values uses
/// the topmost (most-recently-pushed) slot.
#[test]
fn test_stack_tracker_dup_unique_name() {
    let mut t = StackTracker::new();
    let var_x = make_var("x", "uint256", 7);

    t.process_step(0x60, 0, Some(7), &[&var_x]); // push "x" = 100
    t.process_step(0x80, 2, None, &[]); // DUP1 → second "x"
    // Concrete: [100, 100]  (bottom=0, top=1)

    let concrete = vec![u256(100), u256(100)];
    let vals = t.all_variable_values(&concrete);
    // Should deduplicate: only one entry for "x"
    assert_eq!(vals.len(), 1);
    assert_eq!(vals[0].0, "x");
}

// ---------------------------------------------------------------------------
// test_evm_local_vars_simple
// ---------------------------------------------------------------------------

/// Simulate a minimal FlowTest-like scenario entirely in-process:
/// parse a hardcoded AST, build a synthetic structLog, run through the
/// recorder and verify variables are emitted at expected steps.
///
/// Contract skeleton used:
/// ```solidity
/// contract FlowTest {
///   function compute() public returns (uint256) {  // src 50:200:0
///     uint256 a = 10;    // a declared at offset 70
///     uint256 b = 20;    // b declared at offset 95
///     uint256 sum_val = a + b;  // sum_val declared at offset 120
///   }
/// }
/// ```
///
/// We do NOT call the actual recorder here (it needs temp-dir, trace writer,
/// etc.).  Instead we test the AST + StackTracker interaction directly, which
/// is the core M5 logic.
#[test]
fn test_evm_local_vars_simple() {
    // Build a minimal AST representing the `compute` function above.
    let ast_json = r#"{
        "sources": {
            "FlowTest.sol": {
                "AST": {
                    "nodeType": "SourceUnit",
                    "nodes": [{
                        "nodeType": "ContractDefinition",
                        "name": "FlowTest",
                        "nodes": [{
                            "nodeType": "FunctionDefinition",
                            "name": "compute",
                            "kind": "function",
                            "src": "50:200:0",
                            "parameters": { "parameters": [] },
                            "body": {
                                "nodeType": "Block",
                                "statements": [
                                    {
                                        "nodeType": "VariableDeclarationStatement",
                                        "src": "70:14:0",
                                        "declarations": [{
                                            "nodeType": "VariableDeclaration",
                                            "name": "a",
                                            "src": "70:9:0",
                                            "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                        }]
                                    },
                                    {
                                        "nodeType": "VariableDeclarationStatement",
                                        "src": "95:14:0",
                                        "declarations": [{
                                            "nodeType": "VariableDeclaration",
                                            "name": "b",
                                            "src": "95:9:0",
                                            "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                        }]
                                    },
                                    {
                                        "nodeType": "VariableDeclarationStatement",
                                        "src": "120:20:0",
                                        "declarations": [{
                                            "nodeType": "VariableDeclaration",
                                            "name": "sum_val",
                                            "src": "120:15:0",
                                            "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                        }]
                                    }
                                ]
                            }
                        }]
                    }]
                }
            }
        }
    }"#;

    let ast = SolidityAst::from_combined_json(ast_json).unwrap();
    assert_eq!(ast.functions.len(), 1, "should have one function");

    let func = &ast.functions[0];
    assert_eq!(func.name, "compute");
    assert_eq!(func.local_variables.len(), 3);
    assert_eq!(func.local_variables[0].name, "a");
    assert_eq!(func.local_variables[1].name, "b");
    assert_eq!(func.local_variables[2].name, "sum_val");

    // Simulate opcode steps using a StackTracker.
    // Scenario:
    //   Step 1: PUSH1 10  — source offset 70 (matches "a")
    //   Step 2: PUSH1 20  — source offset 95 (matches "b")
    //   Step 3: ADD       — source offset 120 (matches "sum_val" as result)
    //   Step 4: (no new push, sum is on stack)

    let mut tracker = StackTracker::new();

    // Gather in-scope vars at each step.
    // At offset 70: only "a" is in scope (declaration_offset == 70, so `<= 70`).
    let scope_70 = func.vars_in_scope_at(70);
    assert_eq!(scope_70.len(), 1);
    assert_eq!(scope_70[0].name, "a");

    // At offset 100 (after "b"): "a" and "b" should be in scope.
    let scope_100 = func.vars_in_scope_at(100);
    assert_eq!(scope_100.len(), 2);

    // At offset 140 (after "sum_val"): all three vars in scope.
    let scope_140 = func.vars_in_scope_at(140);
    assert_eq!(scope_140.len(), 3);

    // Step 1: PUSH1 10 at source offset 70 → label slot "a"
    let asgn1 = tracker.process_step(0x60, 0, Some(70), &scope_70);
    assert_eq!(asgn1.len(), 1);
    assert_eq!(asgn1[0].name, "a");
    assert_eq!(asgn1[0].stack_position, 0);

    // Step 2: PUSH1 20 at source offset 95 → label slot "b"
    let scope_95 = func.vars_in_scope_at(95);
    let asgn2 = tracker.process_step(0x60, 2, Some(95), &scope_95);
    assert_eq!(asgn2.len(), 1);
    assert_eq!(asgn2[0].name, "b");
    assert_eq!(asgn2[0].stack_position, 1);

    // Concrete stack after 2 pushes: [10, 20]
    let stack_after_push = vec![u256(10), u256(20)];
    assert_eq!(
        tracker.get_variable_value("a", &stack_after_push),
        Some(u256(10))
    );
    assert_eq!(
        tracker.get_variable_value("b", &stack_after_push),
        Some(u256(20))
    );

    // Step 3: ADD at source offset 120 — pops 2, pushes 1 (anonymous result)
    let scope_120 = func.vars_in_scope_at(120);
    // sum_val is now in scope (declaration_offset == 120 <= 120)
    assert_eq!(scope_120.len(), 3);
    // ADD doesn't match any declaration offset (120 != the pushed value's origin)
    let _asgn3 = tracker.process_step(0x01, 4, Some(120), &scope_120);
    // After ADD: depth should be 1 (anonymous sum)
    assert_eq!(tracker.depth(), 1);

    // Concrete stack after ADD: [30]
    let stack_after_add = vec![u256(30)];
    // "a" and "b" are no longer accessible (popped by ADD)
    assert_eq!(tracker.get_variable_value("a", &stack_after_add), None);
    assert_eq!(tracker.get_variable_value("b", &stack_after_add), None);

    eprintln!("test_evm_local_vars_simple passed");
}

/// Verify that vars_in_scope_at correctly handles a function with parameters.
#[test]
fn test_ast_function_with_params_scope() {
    let ast_json = r#"{
        "sources": {
            "T.sol": {
                "AST": {
                    "nodeType": "SourceUnit",
                    "nodes": [{
                        "nodeType": "ContractDefinition",
                        "name": "T",
                        "nodes": [{
                            "nodeType": "FunctionDefinition",
                            "name": "add",
                            "kind": "function",
                            "src": "0:80:0",
                            "parameters": {
                                "parameters": [
                                    {
                                        "nodeType": "VariableDeclaration",
                                        "name": "x",
                                        "src": "10:9:0",
                                        "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                    },
                                    {
                                        "nodeType": "VariableDeclaration",
                                        "name": "y",
                                        "src": "21:9:0",
                                        "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                    }
                                ]
                            },
                            "body": {
                                "nodeType": "Block",
                                "statements": [{
                                    "nodeType": "VariableDeclarationStatement",
                                    "src": "40:16:0",
                                    "declarations": [{
                                        "nodeType": "VariableDeclaration",
                                        "name": "result",
                                        "src": "40:12:0",
                                        "typeName": {"nodeType": "ElementaryTypeName", "name": "uint256"}
                                    }]
                                }]
                            }
                        }]
                    }]
                }
            }
        }
    }"#;

    let ast = SolidityAst::from_combined_json(ast_json).unwrap();
    let func = &ast.functions[0];

    assert_eq!(func.parameters.len(), 2);
    assert_eq!(func.local_variables.len(), 1);

    // Before the local declaration: only params are in scope
    let before_result = func.vars_in_scope_at(35);
    assert_eq!(before_result.len(), 2);
    let names: Vec<&str> = before_result.iter().map(|v| v.name.as_str()).collect();
    assert!(names.contains(&"x"));
    assert!(names.contains(&"y"));

    // After the local declaration: params + result
    let after_result = func.vars_in_scope_at(50);
    assert_eq!(after_result.len(), 3);
}

// ---------------------------------------------------------------------------
// test_evm_local_vars_memory  —  memory-escalated variables
// ---------------------------------------------------------------------------

/// Helper: build a flat EVM memory snapshot of `size` bytes with the free
/// memory pointer word at `0x40` set to `fmp`.
fn mem_with_fmp(size: usize, fmp: u64) -> Vec<u8> {
    let mut m = vec![0u8; size.max(0x60)];
    let off = FREE_MEMORY_POINTER_OFFSET as usize;
    m[off..off + 32].copy_from_slice(&U256::from(fmp).to_be_bytes::<32>());
    m
}

/// Helper: write a 32-byte word into a memory buffer, growing it if needed.
fn write_word(mem: &mut Vec<u8>, at: u64, value: U256) {
    let end = at as usize + 32;
    if mem.len() < end {
        mem.resize(end, 0);
    }
    mem[at as usize..end].copy_from_slice(&value.to_be_bytes::<32>());
}

/// Memory-escalated locals: a struct with two `uint256` fields is allocated
/// in memory via a free-memory-pointer bump, both fields are written via
/// MSTORE, and the first field is read back via MLOAD.
///
/// The test mirrors the contract:
///
/// ```solidity
/// contract MemoryVars {
///   struct Pair { uint256 a; uint256 b; }
///   function compute() public {
///     Pair memory p;       // declared at offset 100
///     p.a = 42;            // at offset 200
///     p.b = 100;           // at offset 240
///     uint256 x = p.a;     // at offset 280
///   }
/// }
/// ```
///
/// We feed the [`MemoryTracker`] a synthetic structLog-style trace covering
/// the relevant opcodes — no solc / anvil required.
#[test]
fn test_evm_local_vars_memory() {
    // -------------------------------------------------------------------
    // 1) AST: a memory-resident local `p` of type "struct Pair memory".
    // -------------------------------------------------------------------
    let var_p = VarDecl {
        name: "p".to_string(),
        type_name: "struct MemoryVars.Pair memory".to_string(),
        src: SourceRange {
            offset: 100,
            length: 20,
            file_index: 0,
        },
        declaration_offset: 100,
        statement_range: Some(SourceRange {
            offset: 100,
            length: 200, // covers all field-assignment statements below
            file_index: 0,
        }),
    };

    // The classifier should recognise this as memory-resident.
    assert!(
        var_p.is_memory_resident(),
        "struct memory local must be flagged"
    );

    let scope: Vec<&VarDecl> = vec![&var_p];

    // -------------------------------------------------------------------
    // 2) Build the synthetic memory buffer and drive the MemoryTracker.
    // -------------------------------------------------------------------
    let mut tracker = MemoryTracker::new();

    // Pre-allocation memory: FMP = 0x80, nothing else written.
    let mut memory = mem_with_fmp(0x100, INITIAL_FREE_MEMORY_POINTER);

    // ---- Step A: MSTORE bumps the free-memory pointer ----
    //
    // Solidity emits `MSTORE 0x40, 0xC0` to reserve 0x40 bytes
    // (= sizeof(struct Pair)) starting at the old FMP (0x80).
    //
    // structLog convention: `stack` is bottom-to-top, so the pre-step
    // stack for MSTORE is [..., value, dest].  Top of stack is `dest=0x40`,
    // one below is `value=0xC0` (the new FMP).
    let bump_stack = vec![
        U256::from(0xC0u64),                    // value: new FMP
        U256::from(FREE_MEMORY_POINTER_OFFSET), // dest: 0x40
    ];
    let asgn_a = tracker.process_step(
        0x52, // MSTORE
        /*pc*/ 100,
        /*source_offset*/ Some(100),
        &scope,
        &bump_stack,
        &memory,
    );
    assert!(
        asgn_a.is_empty(),
        "the FMP bump itself emits no field-level assignment"
    );
    assert_eq!(tracker.region_count(), 1, "one region registered");
    let region = tracker.region("p").expect("p region registered");
    assert_eq!(region.base_offset, INITIAL_FREE_MEMORY_POINTER);
    assert_eq!(region.size_in_bytes, 0x40);

    // Update memory: FMP word at 0x40 is now 0xC0.
    write_word(&mut memory, FREE_MEMORY_POINTER_OFFSET, U256::from(0xC0u64));

    // ---- Step B: MSTORE 0x80, 42 (writes p.a = 42) ----
    let store_a_stack = vec![U256::from(42u64), U256::from(0x80u64)];
    let asgn_b = tracker.process_step(0x52, 110, Some(200), &scope, &store_a_stack, &memory);
    assert_eq!(asgn_b.len(), 1, "MSTORE inside region emits an assignment");
    assert_eq!(asgn_b[0].name, "p");
    assert_eq!(asgn_b[0].memory_offset, 0x80);
    assert_eq!(asgn_b[0].size, 32);

    // Reflect the write in the memory snapshot.
    write_word(&mut memory, 0x80, U256::from(42u64));

    // ---- Step C: MSTORE 0xA0, 100 (writes p.b = 100) ----
    let store_b_stack = vec![U256::from(100u64), U256::from(0xA0u64)];
    let asgn_c = tracker.process_step(0x52, 120, Some(240), &scope, &store_b_stack, &memory);
    assert_eq!(asgn_c.len(), 1);
    assert_eq!(asgn_c[0].name, "p");
    assert_eq!(asgn_c[0].memory_offset, 0xA0);
    assert_eq!(asgn_c[0].size, 32);

    write_word(&mut memory, 0xA0, U256::from(100u64));

    // ---- Step D: MLOAD 0x80 (reads p.a) ----
    //
    // MLOAD's only effect on the tracker is to observe the memory
    // snapshot; the value surfaces via get_field_value / get_variable_value.
    let mload_stack = vec![U256::from(0x80u64)];
    let asgn_d = tracker.process_step(0x51, 130, Some(280), &scope, &mload_stack, &memory);
    assert!(asgn_d.is_empty(), "MLOAD never produces an assignment");

    // -------------------------------------------------------------------
    // 3) Surface the variable values from the memory snapshot.
    // -------------------------------------------------------------------
    // Header word (field "a") matches the first MSTORE.
    assert_eq!(
        tracker.get_variable_value("p", &memory),
        Some(U256::from(42u64)),
        "p.a should be 42 (header word of the region)"
    );

    // Field at offset 0 within p == field "a"
    assert_eq!(
        tracker.get_field_value("p", 0, &memory),
        Some(U256::from(42u64)),
    );

    // Field at offset 0x20 (32) within p == field "b"
    assert_eq!(
        tracker.get_field_value("p", 0x20, &memory),
        Some(U256::from(100u64)),
    );

    // Field offset past the region's allocated size must return None.
    assert_eq!(tracker.get_field_value("p", 0x40, &memory), None);

    // all_variable_values reports the one tracked region with its header.
    let all = tracker.all_variable_values(&memory);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].0, "p");
    assert_eq!(all[0].1, U256::from(42u64));

    // -------------------------------------------------------------------
    // 4) Sanity: looking up a variable that was never tracked returns None.
    // -------------------------------------------------------------------
    assert_eq!(tracker.get_variable_value("does_not_exist", &memory), None);
    assert_eq!(tracker.region("does_not_exist").is_none(), true);

    eprintln!("test_evm_local_vars_memory passed");
}

/// A dynamic `uint256[] memory` local is also a memory-escalated variable.
/// Verify the type classifier and tracker handle the array-of-words pattern.
#[test]
fn test_evm_local_vars_memory_dynamic_array() {
    // `uint256[] memory arr` declared at source offset 50, statement-span
    // covering all field writes below.
    let var_arr = VarDecl {
        name: "arr".to_string(),
        type_name: "uint256[] memory".to_string(),
        src: SourceRange {
            offset: 50,
            length: 20,
            file_index: 0,
        },
        declaration_offset: 50,
        statement_range: Some(SourceRange {
            offset: 50,
            length: 300,
            file_index: 0,
        }),
    };
    assert!(var_arr.is_memory_resident());

    let scope: Vec<&VarDecl> = vec![&var_arr];

    let mut tracker = MemoryTracker::new();
    let mut memory = mem_with_fmp(0x200, INITIAL_FREE_MEMORY_POINTER);

    // Allocate 3 words: 1 for length + 2 for elements ⇒ 0x60 bytes.
    let new_fmp = INITIAL_FREE_MEMORY_POINTER + 0x60;
    let bump_stack = vec![U256::from(new_fmp), U256::from(FREE_MEMORY_POINTER_OFFSET)];
    let _ = tracker.process_step(0x52, 0, Some(50), &scope, &bump_stack, &memory);
    write_word(&mut memory, FREE_MEMORY_POINTER_OFFSET, U256::from(new_fmp));

    // Write length = 2 at base.
    let store_len = vec![U256::from(2u64), U256::from(INITIAL_FREE_MEMORY_POINTER)];
    let asgn_len = tracker.process_step(0x52, 4, Some(60), &scope, &store_len, &memory);
    assert_eq!(asgn_len.len(), 1);
    assert_eq!(asgn_len[0].name, "arr");
    write_word(&mut memory, INITIAL_FREE_MEMORY_POINTER, U256::from(2u64));

    // Write element 0 = 777 at base + 0x20.
    let elem0_off = INITIAL_FREE_MEMORY_POINTER + 0x20;
    let store_elem0 = vec![U256::from(777u64), U256::from(elem0_off)];
    let asgn0 = tracker.process_step(0x52, 8, Some(70), &scope, &store_elem0, &memory);
    assert_eq!(asgn0.len(), 1);
    write_word(&mut memory, elem0_off, U256::from(777u64));

    // Surface the length and element 0 from the snapshot.
    assert_eq!(
        tracker.get_variable_value("arr", &memory),
        Some(U256::from(2u64)),
        "header word = array length"
    );
    assert_eq!(
        tracker.get_field_value("arr", 0x20, &memory),
        Some(U256::from(777u64)),
        "element 0"
    );
}

// ---------------------------------------------------------------------------
// Integration smoke-test: recorder accepts SolidityAst parameter
// ---------------------------------------------------------------------------

/// Verify that EvmRecorder::record_from_structlog compiles and runs with a
/// SolidityAst passed in, using completely synthetic (trivial) data.
#[test]
fn test_recorder_accepts_solidity_ast() {
    use codetracer_evm_recorder::recorder::EvmRecorder;
    use codetracer_evm_recorder::source_map::SourceMap;
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let mut recorder = EvmRecorder::new("test", tmp.path()).unwrap();
    recorder.initialize().unwrap();

    // Empty inputs — should complete without panic.
    let source_map = SourceMap::parse("");
    let bytecode: &[u8] = &[];
    let source_path = std::path::Path::new("test.sol");
    let source_contents = "";
    let ast = SolidityAst::default();

    recorder
        .record_from_structlog(
            &[],
            &source_map,
            bytecode,
            &[source_path],
            &[source_contents],
            None,
            Some(&ast),
        )
        .unwrap();

    recorder.finalize().unwrap();
    eprintln!("test_recorder_accepts_solidity_ast passed");
}
