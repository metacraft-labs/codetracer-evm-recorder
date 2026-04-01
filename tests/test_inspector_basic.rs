//! Unit tests for CodeTracerInspector with a simple in-memory revm EVM.
//!
//! These tests do NOT require any external tooling (no anvil, no RPC).

use codetracer_evm_recorder::inspector::{CallKind, CodeTracerInspector};
use revm::{
    context::{Context, TxEnv},
    database::InMemoryDB,
    handler::{MainBuilder, MainContext},
    inspector::InspectEvm,
    primitives::{TxKind, U256, address},
    state::{AccountInfo, Bytecode, bytecode::opcode},
};

// Re-use the BenchmarkDB from revm's database crate for simple tests.
use revm::database::BenchmarkDB;

/// Helper: run the given bytecode with CodeTracerInspector and return the inspector.
fn run_bytecode(code: Vec<u8>) -> CodeTracerInspector {
    let bytecode = Bytecode::new_raw(revm::primitives::Bytes::from(code));
    let ctx = Context::mainnet().with_db(BenchmarkDB::new_bytecode(bytecode));
    // Build the EVM with the inspector up-front; use `inspect_one_tx` (no second
    // inspector argument) so the same inspector instance is used throughout and
    // we don't discard the captured data from a first instance.
    let mut evm = ctx.build_mainnet_with_inspector(CodeTracerInspector::new());

    let _ = evm.inspect_one_tx(
        TxEnv::builder()
            .caller(address!("0000000000000000000000000000000000000001"))
            .kind(TxKind::Call(
                // BenchmarkDB target is the pre-defined BENCH_TARGET constant
                revm::database::BENCH_TARGET,
            ))
            .gas_limit(100_000)
            .build_fill(),
    );

    evm.inspector
}

// -------------------------------------------------------------------------
// Test 1: Basic step recording
// -------------------------------------------------------------------------
#[test]
fn test_inspector_basic_steps() {
    // PUSH1 0x42, PUSH1 0x01, ADD, STOP
    let code = vec![
        opcode::PUSH1,
        0x42,
        opcode::PUSH1,
        0x01,
        opcode::ADD,
        opcode::STOP,
    ];

    let inspector = run_bytecode(code);
    let data = inspector.execution_data();

    // We expect at least 4 steps: PUSH1, PUSH1, ADD, STOP
    assert!(
        data.step_count() >= 4,
        "expected >= 4 steps, got {}",
        data.step_count()
    );

    // First step should be PUSH1
    let first = &data.steps[0];
    assert_eq!(first.opcode_name, "PUSH1", "first opcode should be PUSH1");
    assert_eq!(first.pc, 0, "first step should be at PC=0");
    assert_eq!(
        first.stack.len(),
        0,
        "stack should be empty before first PUSH1"
    );

    // Second step should also be PUSH1 (at PC=2, after PUSH1 + 1-byte immediate)
    let second = &data.steps[1];
    assert_eq!(second.opcode_name, "PUSH1", "second opcode should be PUSH1");
    assert_eq!(
        second.stack.len(),
        1,
        "stack should have 1 element before second PUSH1"
    );
    assert_eq!(
        second.stack[0],
        U256::from(0x42u8),
        "stack[0] should be 0x42"
    );

    // Third step is ADD
    let add_step = data.steps.iter().find(|s| s.opcode_name == "ADD");
    assert!(add_step.is_some(), "ADD opcode should be recorded");
    let add_step = add_step.unwrap();
    assert_eq!(add_step.stack.len(), 2, "ADD should see 2 values on stack");
}

// -------------------------------------------------------------------------
// Test 2: Memory is tracked
// -------------------------------------------------------------------------
#[test]
fn test_inspector_memory_tracking() {
    // PUSH1 0x42, PUSH1 0x00, MSTORE, STOP
    let code = vec![
        opcode::PUSH1,
        0x42,
        opcode::PUSH1,
        0x00,
        opcode::MSTORE,
        opcode::STOP,
    ];

    let inspector = run_bytecode(code);
    let data = inspector.execution_data();

    // After MSTORE the memory should grow to at least 32 bytes
    let mstore_step = data.steps.iter().find(|s| s.opcode_name == "MSTORE");
    assert!(mstore_step.is_some(), "MSTORE step should be recorded");

    // The step *after* MSTORE (STOP) should reflect memory size > 0.
    // (The step records state BEFORE the instruction; check STOP has non-zero memory)
    let stop_step = data.steps.iter().find(|s| s.opcode_name == "STOP");
    assert!(stop_step.is_some(), "STOP step should be recorded");
    assert!(
        stop_step.unwrap().memory_size >= 32,
        "memory should be at least 32 bytes after MSTORE"
    );
}

// -------------------------------------------------------------------------
// Test 3: LOG0 is captured
// -------------------------------------------------------------------------
#[test]
fn test_inspector_log_capture() {
    // PUSH1 0x42, PUSH1 0x00, MSTORE, PUSH1 0x20, PUSH1 0x00, LOG0, STOP
    let code = vec![
        opcode::PUSH1,
        0x42,
        opcode::PUSH1,
        0x00,
        opcode::MSTORE,
        opcode::PUSH1,
        0x20, // size
        opcode::PUSH1,
        0x00, // offset
        opcode::LOG0,
        opcode::STOP,
    ];

    let inspector = run_bytecode(code);
    let data = inspector.execution_data();

    assert_eq!(data.logs.len(), 1, "one LOG0 event should be captured");
    let log = &data.logs[0];
    assert_eq!(log.topics.len(), 0, "LOG0 has 0 topics");
    assert_eq!(
        log.data.len(),
        32,
        "LOG0 data should be 32 bytes (MSTORE slot)"
    );
}

// -------------------------------------------------------------------------
// Test 4: CALL is captured with correct metadata
// -------------------------------------------------------------------------
#[test]
fn test_inspector_call_capture() {
    // Contract A calls contract B
    // Callee just returns immediately
    let callee_code = revm::primitives::Bytes::from(vec![opcode::STOP]);
    let callee_addr = address!("0000000000000000000000000000000000000001");

    // Caller does: PUSH1 0, PUSH1 0, PUSH1 0, PUSH1 0, PUSH1 0, PUSH20 <callee>, PUSH2 0xFFFF, CALL, STOP
    let mut caller_code = vec![
        opcode::PUSH1,
        0x00, // retSize
        opcode::PUSH1,
        0x00, // retOffset
        opcode::PUSH1,
        0x00, // argsSize
        opcode::PUSH1,
        0x00, // argsOffset
        opcode::PUSH1,
        0x00,           // value
        opcode::PUSH20, // 20-byte callee address
    ];
    caller_code.extend_from_slice(callee_addr.as_slice());
    caller_code.extend_from_slice(&[
        opcode::PUSH2,
        0xFF,
        0xFF, // gas
        opcode::CALL,
        opcode::STOP,
    ]);

    let mut db = InMemoryDB::default();

    let caller_addr = revm::database::BENCH_TARGET;

    db.insert_account_info(
        caller_addr,
        AccountInfo {
            balance: U256::from(1_000_000u64),
            nonce: 0,
            code_hash: revm::primitives::keccak256(&caller_code),
            code: Some(Bytecode::new_raw(revm::primitives::Bytes::from(
                caller_code,
            ))),
            account_id: None,
        },
    );
    db.insert_account_info(
        callee_addr,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 0,
            code_hash: revm::primitives::keccak256(&callee_code),
            code: Some(Bytecode::new_raw(callee_code)),
            account_id: None,
        },
    );

    let ctx = Context::mainnet().with_db(db);
    let mut evm = ctx.build_mainnet_with_inspector(CodeTracerInspector::new());

    let _ = evm.inspect_one_tx(
        TxEnv::builder()
            .caller(address!("0000000000000000000000000000000000000002"))
            .kind(TxKind::Call(caller_addr))
            .gas_limit(200_000)
            .build_fill(),
    );

    let data = &evm.inspector.execution_data();

    // Should have recorded at least two CALL events:
    // - index 0: the top-level call (BENCH_CALLER -> caller_addr / BENCH_TARGET)
    // - index 1: the inner CALL (caller_addr -> callee_addr)
    assert!(
        data.calls.len() >= 2,
        "at least 2 CALL events should be recorded (top-level + inner)"
    );

    // Find the inner call targeting callee_addr
    let inner_call = data.calls.iter().find(|c| c.target == callee_addr);
    assert!(
        inner_call.is_some(),
        "should have a CALL targeting callee_addr {:?}; got calls: {:?}",
        callee_addr,
        data.calls.iter().map(|c| c.target).collect::<Vec<_>>()
    );
    let inner_call = inner_call.unwrap();
    assert_eq!(
        inner_call.kind,
        CallKind::Call,
        "inner call kind should be CALL"
    );
    assert_eq!(inner_call.value, U256::ZERO, "inner call value should be 0");

    // Steps should be non-empty
    assert!(data.step_count() > 0, "steps should be recorded");
}

// -------------------------------------------------------------------------
// Test 5: CREATE is captured
// -------------------------------------------------------------------------
#[test]
fn test_inspector_create_capture() {
    // init code: STOP (deploying an empty contract)
    let init_code: Vec<u8> = vec![opcode::STOP];

    // deployer code: store init_code in memory, then CREATE
    let mut deployer = vec![
        opcode::PUSH1,
        init_code.len() as u8, // size
        opcode::PUSH1,
        0x0C, // code offset (after CREATE params below)
        opcode::PUSH1,
        0x00, // memory dest offset
        opcode::CODECOPY,
        // CREATE(value=0, offset=0, size=len(init_code))
        opcode::PUSH1,
        init_code.len() as u8,
        opcode::PUSH1,
        0x00,
        opcode::PUSH1,
        0x00,
        opcode::CREATE,
        opcode::STOP,
    ];
    deployer.extend_from_slice(&init_code);

    let inspector = run_bytecode(deployer);
    let data = inspector.execution_data();

    let create_events: Vec<_> = data
        .calls
        .iter()
        .filter(|c| c.kind == CallKind::Create)
        .collect();
    assert!(!create_events.is_empty(), "CREATE event should be recorded");
    assert_eq!(
        create_events[0].value,
        U256::ZERO,
        "CREATE value should be 0"
    );
}

// -------------------------------------------------------------------------
// Test 6: Depth tracking
// -------------------------------------------------------------------------
#[test]
fn test_inspector_depth_tracking() {
    // Simple bytecode with no sub-calls — all steps should be at the same depth.
    // The top-level call() hook fires before the first step, incrementing depth to 1.
    let code = vec![opcode::PUSH1, 0x01, opcode::POP, opcode::STOP];
    let inspector = run_bytecode(code);
    let data = inspector.execution_data();

    // All steps should be at the same depth (depth 1 = inside the top-level call frame)
    let depths: Vec<u64> = data.steps.iter().map(|s| s.depth).collect();
    assert!(!depths.is_empty(), "should have recorded some steps");
    let first_depth = depths[0];
    for (i, d) in depths.iter().enumerate() {
        assert_eq!(
            *d, first_depth,
            "step {} depth {} should match first step depth {}",
            i, d, first_depth
        );
    }
}

// -------------------------------------------------------------------------
// Test 7: Opcode names are correct strings
// -------------------------------------------------------------------------
#[test]
fn test_inspector_opcode_names() {
    // PUSH1 0xFF, DUP1, ADD, STOP
    let code = vec![opcode::PUSH1, 0xFF, opcode::DUP1, opcode::ADD, opcode::STOP];
    let inspector = run_bytecode(code);
    let data = inspector.execution_data();

    let names: Vec<&str> = data.steps.iter().map(|s| s.opcode_name.as_str()).collect();
    assert!(names.contains(&"PUSH1"), "PUSH1 should be named PUSH1");
    assert!(names.contains(&"DUP1"), "DUP1 should be named DUP1");
    assert!(names.contains(&"ADD"), "ADD should be named ADD");
    assert!(names.contains(&"STOP"), "STOP should be named STOP");
}
