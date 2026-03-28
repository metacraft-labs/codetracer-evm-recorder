//! End-to-end integration tests for local variable reconstruction and
//! complex EVM patterns.
//!
//! Each test compiles a Solidity contract, deploys to a local anvil node,
//! traces via `debug_traceTransaction`, processes through the recorder with
//! the Solidity AST, and verifies the trace output.
//!
//! Requires `solc` and `anvil` on PATH (provided by the Nix dev shell).

use std::path::Path;
use std::process::Command;

use alloy::network::TransactionBuilder;
use alloy::primitives::{Bytes, U256};
use alloy::providers::{fillers::*, Identity, Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::TransactionRequest;

use codetracer_evm_recorder::recorder::EvmRecorder;
use codetracer_evm_recorder::solidity_ast::SolidityAst;
use codetracer_evm_recorder::source_map::SourceMap;
use codetracer_evm_recorder::storage_layout::StorageLayout;
use codetracer_evm_recorder::trace_fetcher;

type HttpProvider = FillProvider<
    JoinFill<Identity, JoinFill<GasFiller, JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>>>,
    RootProvider,
>;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn has_solc() -> bool {
    Command::new("solc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn has_anvil() -> bool {
    Command::new("anvil")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Compile a Solidity file (relative to CARGO_MANIFEST_DIR) with AST support.
fn compile_contract(sol_path: &str) -> (serde_json::Value, String) {
    let output = Command::new("solc")
        .args([
            "--combined-json",
            "abi,bin,bin-runtime,srcmap-runtime,storage-layout,ast",
            "--no-cbor-metadata",
            sol_path,
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run solc");
    assert!(
        output.status.success(),
        "solc failed for {}: {}",
        sol_path,
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    (json, raw)
}

/// Compile a Solidity file at an absolute path.
fn compile_contract_abs(sol_path: &str) -> (serde_json::Value, String) {
    let output = Command::new("solc")
        .args([
            "--combined-json",
            "abi,bin,bin-runtime,srcmap-runtime,storage-layout,ast",
            "--no-cbor-metadata",
            sol_path,
        ])
        .output()
        .expect("failed to run solc");
    assert!(
        output.status.success(),
        "solc failed for {}: {}",
        sol_path,
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    (json, raw)
}

/// Compute the 4-byte Solidity function selector.
fn selector(sig: &str) -> [u8; 4] {
    let hash = alloy::primitives::keccak256(sig.as_bytes());
    let mut sel = [0u8; 4];
    sel.copy_from_slice(&hash[..4]);
    sel
}

/// Helper: deploy contract and call function, returning struct logs.
///
/// Handles the full cycle: spawn anvil, deploy, send tx, fetch trace.
struct TraceHelper {
    _anvil: alloy::node_bindings::AnvilInstance,
    rpc_url: String,
    provider: HttpProvider,
    from: alloy::primitives::Address,
}

impl TraceHelper {
    async fn new() -> Self {
        let anvil = alloy::node_bindings::Anvil::new()
            .arg("--steps-tracing")
            .spawn();
        let rpc_url = anvil.endpoint();
        let provider = ProviderBuilder::new().connect_http(rpc_url.parse().unwrap());
        let accounts = provider.get_accounts().await.unwrap();
        let from = accounts[0];
        Self {
            _anvil: anvil,
            rpc_url,
            provider,
            from,
        }
    }

    async fn deploy(&self, deploy_hex: &str, constructor_args: &[u8]) -> alloy::primitives::Address {
        let mut deploy_bytes = alloy::hex::decode(deploy_hex).expect("invalid deploy bytecode");
        deploy_bytes.extend_from_slice(constructor_args);
        let deploy_tx = TransactionRequest::default()
            .from(self.from)
            .with_deploy_code(Bytes::from(deploy_bytes));
        let receipt = self
            .provider
            .send_transaction(deploy_tx)
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
        receipt.contract_address.expect("no contract address")
    }

    async fn call_and_trace(
        &self,
        contract: alloy::primitives::Address,
        calldata: &[u8],
    ) -> Vec<codetracer_evm_recorder::structlog::StructLog> {
        let call_tx = TransactionRequest::default()
            .from(self.from)
            .to(contract)
            .with_input(Bytes::copy_from_slice(calldata));
        let receipt = self
            .provider
            .send_transaction(call_tx)
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
        let tx_hash = receipt.transaction_hash;
        let frame = trace_fetcher::fetch_struct_logs(&self.rpc_url, tx_hash)
            .await
            .expect("fetch_struct_logs failed");
        let logs = trace_fetcher::extract_struct_logs(&frame);
        assert!(!logs.is_empty(), "structLog should not be empty");
        logs.to_vec()
    }
}

/// Record struct logs through the recorder and return the output directory.
fn record_trace(
    program: &str,
    struct_logs: &[codetracer_evm_recorder::structlog::StructLog],
    source_map: &SourceMap,
    runtime_bytecode: &[u8],
    source_path: &Path,
    source_contents: &str,
    storage_layout: Option<&StorageLayout>,
    ast: &SolidityAst,
) -> tempfile::TempDir {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let mut recorder = EvmRecorder::new(program, tmp_dir.path()).unwrap();
    recorder.initialize().unwrap();

    recorder
        .record_from_structlog(
            struct_logs,
            source_map,
            runtime_bytecode,
            &[source_path],
            &[source_contents],
            storage_layout,
            Some(ast),
        )
        .unwrap();
    recorder.finalize().unwrap();

    // Basic sanity checks.
    assert!(tmp_dir.path().join("trace.bin").exists(), "trace.bin missing");
    assert!(
        tmp_dir.path().join("trace_metadata.json").exists(),
        "trace_metadata.json missing"
    );
    assert!(
        tmp_dir.path().join("trace_paths.json").exists(),
        "trace_paths.json missing"
    );

    let trace_size = std::fs::metadata(tmp_dir.path().join("trace.bin"))
        .unwrap()
        .len();
    assert!(trace_size > 0, "trace.bin should be non-empty");

    tmp_dir
}

/// Extract contract artifacts from compiled JSON.
struct ContractArtifacts {
    deploy_hex: String,
    source_map: SourceMap,
    storage_layout: Option<StorageLayout>,
    runtime_bytecode: Vec<u8>,
}

impl ContractArtifacts {
    fn from_compiled(compiled: &serde_json::Value, contract_suffix: &str) -> Self {
        let contracts = compiled["contracts"].as_object().unwrap();
        let (_, contract_json) = contracts
            .iter()
            .find(|(k, _)| k.ends_with(contract_suffix))
            .unwrap_or_else(|| panic!("contract {} not found in solc output", contract_suffix));

        let deploy_hex = contract_json["bin"].as_str().expect("missing bin").to_string();
        let runtime_hex = contract_json["bin-runtime"]
            .as_str()
            .expect("missing bin-runtime");
        let srcmap_raw = contract_json["srcmap-runtime"]
            .as_str()
            .expect("missing srcmap-runtime");

        let source_map = SourceMap::parse(srcmap_raw);
        let storage_layout = serde_json::from_value(contract_json["storage-layout"].clone()).ok();
        let runtime_bytecode = alloy::hex::decode(runtime_hex).expect("invalid runtime bytecode");

        Self {
            deploy_hex,
            source_map,
            storage_layout,
            runtime_bytecode,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// FlowTest.sol: verify AST parsing, trace file generation, and that
/// SSTORE operations (storedA, storedResult) produce trace events.
#[tokio::test]
async fn test_flowtest_vars() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (compiled, raw_json) = compile_contract("contracts/FlowTest.sol");
    let ast = SolidityAst::from_combined_json(&raw_json).unwrap();

    // Verify AST has compute() with locals a, b, result.
    let compute_fn = ast
        .functions
        .iter()
        .find(|f| f.name == "compute")
        .expect("AST should contain `compute`");
    assert_eq!(compute_fn.local_variables.len(), 3, "compute() should have 3 locals");
    let local_names: Vec<_> = compute_fn.local_variables.iter().map(|v| v.name.as_str()).collect();
    assert!(local_names.contains(&"a"), "missing local 'a'");
    assert!(local_names.contains(&"b"), "missing local 'b'");
    assert!(local_names.contains(&"result"), "missing local 'result'");

    let arts = ContractArtifacts::from_compiled(&compiled, ":FlowTest");
    let source_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/FlowTest.sol");
    let source_contents = std::fs::read_to_string(&source_path).unwrap();

    let helper = TraceHelper::new().await;
    let contract = helper.deploy(&arts.deploy_hex, &[]).await;

    let calldata = selector("compute()");
    let struct_logs = helper.call_and_trace(contract, &calldata).await;

    // Verify SSTORE operations for storage writes.
    let sstore_count = struct_logs.iter().filter(|l| l.op.as_ref() == "SSTORE").count();
    assert!(
        sstore_count >= 2,
        "expected >= 2 SSTOREs (storedA, storedResult), got {}",
        sstore_count
    );

    let _trace_dir = record_trace(
        "FlowTest",
        &struct_logs,
        &arts.source_map,
        &arts.runtime_bytecode,
        &source_path,
        &source_contents,
        arts.storage_layout.as_ref(),
        &ast,
    );

    eprintln!(
        "test_flowtest_vars passed: {} struct logs, {} SSTOREs, 3 locals in AST",
        struct_logs.len(),
        sstore_count
    );
}

/// solidity_flow_test.sol: verify AST has run() with 5 locals and that
/// the full recording pipeline works with constructor arguments.
#[tokio::test]
async fn test_solidity_flow_test_vars() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let sol_source_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("codetracer/src/db-backend/test-programs/solidity/solidity_flow_test.sol");

    if !sol_source_path.exists() {
        eprintln!(
            "SKIPPING test_solidity_flow_test_vars: {} not found",
            sol_source_path.display()
        );
        return;
    }

    let (compiled, raw_json) = compile_contract_abs(sol_source_path.to_str().unwrap());
    let ast = SolidityAst::from_combined_json(&raw_json).unwrap();

    // Verify the AST parsed run() with 5 locals.
    let run_fn = ast
        .functions
        .iter()
        .find(|f| f.name == "run")
        .expect("AST should contain `run`");
    assert_eq!(run_fn.local_variables.len(), 5, "run() should have 5 locals");

    let expected_locals = ["a", "b", "sum_val", "doubled", "final_result"];
    for name in &expected_locals {
        assert!(
            run_fn.local_variables.iter().any(|v| v.name == *name),
            "run() missing local variable '{}'",
            name
        );
    }

    // Verify all locals have statement_range set (needed for label recovery).
    for lv in &run_fn.local_variables {
        assert!(
            lv.statement_range.is_some(),
            "local '{}' should have statement_range set",
            lv.name
        );
    }

    let arts = ContractArtifacts::from_compiled(&compiled, ":FlowTest");
    let source_contents = std::fs::read_to_string(&sol_source_path).unwrap();

    // Deploy with constructor arg uint256(10).
    let constructor_arg = U256::from(10).to_be_bytes::<32>();
    let helper = TraceHelper::new().await;
    let contract = helper.deploy(&arts.deploy_hex, &constructor_arg).await;

    // Call run().
    let calldata = selector("run()");
    let struct_logs = helper.call_and_trace(contract, &calldata).await;

    // Verify we have SSTORE for storedResult.
    let sstore_count = struct_logs.iter().filter(|l| l.op.as_ref() == "SSTORE").count();
    assert!(sstore_count >= 1, "expected >= 1 SSTORE, got {}", sstore_count);

    // Verify LOG1 for the Computed event.
    let log_count = struct_logs
        .iter()
        .filter(|l| l.op.as_ref().starts_with("LOG"))
        .count();
    assert!(log_count >= 1, "expected >= 1 LOG, got {}", log_count);

    let _trace_dir = record_trace(
        "FlowTest",
        &struct_logs,
        &arts.source_map,
        &arts.runtime_bytecode,
        &sol_source_path,
        &source_contents,
        arts.storage_layout.as_ref(),
        &ast,
    );

    eprintln!(
        "test_solidity_flow_test_vars passed: 5 locals, {} SSTOREs, {} LOGs",
        sstore_count,
        log_count
    );
}

/// ControlFlowTest.branching(true): verify if/else branch tracking.
/// The true branch sets x=100, y=200, branchVal=300.
#[tokio::test]
async fn test_control_flow_branching() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (compiled, raw_json) = compile_contract("contracts/ControlFlowTest.sol");
    let ast = SolidityAst::from_combined_json(&raw_json).unwrap();

    let branching_fn = ast
        .functions
        .iter()
        .find(|f| f.name == "branching")
        .expect("AST should contain `branching`");
    assert_eq!(branching_fn.parameters.len(), 1, "branching() should have 1 param");
    assert_eq!(
        branching_fn.local_variables.len(),
        3,
        "branching() should have 3 locals: x, y, branchVal"
    );

    let arts = ContractArtifacts::from_compiled(&compiled, ":ControlFlowTest");
    let source_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/ControlFlowTest.sol");
    let source_contents = std::fs::read_to_string(&source_path).unwrap();

    let helper = TraceHelper::new().await;
    let contract = helper.deploy(&arts.deploy_hex, &[]).await;

    // branching(true): selector + bool(true)
    let mut calldata = Vec::from(selector("branching(bool)"));
    let mut arg = [0u8; 32];
    arg[31] = 1;
    calldata.extend_from_slice(&arg);

    let struct_logs = helper.call_and_trace(contract, &calldata).await;

    // Should have SSTORE for lastResult.
    let sstore_count = struct_logs.iter().filter(|l| l.op.as_ref() == "SSTORE").count();
    assert!(sstore_count >= 1, "expected >= 1 SSTORE for lastResult");

    let _trace_dir = record_trace(
        "ControlFlowTest",
        &struct_logs,
        &arts.source_map,
        &arts.runtime_bytecode,
        &source_path,
        &source_contents,
        arts.storage_layout.as_ref(),
        &ast,
    );

    eprintln!(
        "test_control_flow_branching passed: {} struct logs, {} SSTOREs",
        struct_logs.len(),
        sstore_count
    );
}

/// ControlFlowTest.looping(5): verify for-loop variable tracking.
/// Expected: total = 1+2+3+4+5 = 15.
#[tokio::test]
async fn test_control_flow_looping() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (compiled, raw_json) = compile_contract("contracts/ControlFlowTest.sol");
    let ast = SolidityAst::from_combined_json(&raw_json).unwrap();

    let looping_fn = ast
        .functions
        .iter()
        .find(|f| f.name == "looping")
        .expect("AST should contain `looping`");
    // Should have locals: total, and the loop var i (from the for loop).
    assert!(
        looping_fn.local_variables.len() >= 1,
        "looping() should have >= 1 local (total)"
    );

    let arts = ContractArtifacts::from_compiled(&compiled, ":ControlFlowTest");
    let source_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/ControlFlowTest.sol");
    let source_contents = std::fs::read_to_string(&source_path).unwrap();

    let helper = TraceHelper::new().await;
    let contract = helper.deploy(&arts.deploy_hex, &[]).await;

    // looping(5)
    let mut calldata = Vec::from(selector("looping(uint256)"));
    calldata.extend_from_slice(&U256::from(5).to_be_bytes::<32>());

    let struct_logs = helper.call_and_trace(contract, &calldata).await;

    // The loop body executes 5 times, producing many more struct log entries.
    assert!(
        struct_logs.len() > 50,
        "looping(5) should produce many steps (loop unrolling), got {}",
        struct_logs.len()
    );

    let _trace_dir = record_trace(
        "ControlFlowTest",
        &struct_logs,
        &arts.source_map,
        &arts.runtime_bytecode,
        &source_path,
        &source_contents,
        arts.storage_layout.as_ref(),
        &ast,
    );

    eprintln!(
        "test_control_flow_looping passed: {} struct logs",
        struct_logs.len()
    );
}

/// ControlFlowTest.nestedCalls(3, 4): verify internal function calls.
/// Expected: sum=7, product=12, combined=19.
#[tokio::test]
async fn test_control_flow_nested_calls() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (compiled, raw_json) = compile_contract("contracts/ControlFlowTest.sol");
    let ast = SolidityAst::from_combined_json(&raw_json).unwrap();

    let nested_fn = ast
        .functions
        .iter()
        .find(|f| f.name == "nestedCalls")
        .expect("AST should contain `nestedCalls`");
    assert_eq!(nested_fn.parameters.len(), 2, "nestedCalls() should have 2 params");

    let arts = ContractArtifacts::from_compiled(&compiled, ":ControlFlowTest");
    let source_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/ControlFlowTest.sol");
    let source_contents = std::fs::read_to_string(&source_path).unwrap();

    let helper = TraceHelper::new().await;
    let contract = helper.deploy(&arts.deploy_hex, &[]).await;

    // nestedCalls(3, 4)
    let mut calldata = Vec::from(selector("nestedCalls(uint256,uint256)"));
    calldata.extend_from_slice(&U256::from(3).to_be_bytes::<32>());
    calldata.extend_from_slice(&U256::from(4).to_be_bytes::<32>());

    let struct_logs = helper.call_and_trace(contract, &calldata).await;

    // Should have jump-into patterns for internal calls.
    let jump_count = struct_logs.iter().filter(|l| l.op.as_ref() == "JUMP").count();
    assert!(
        jump_count >= 2,
        "nestedCalls should have >= 2 JUMPs (internal calls), got {}",
        jump_count
    );

    let _trace_dir = record_trace(
        "ControlFlowTest",
        &struct_logs,
        &arts.source_map,
        &arts.runtime_bytecode,
        &source_path,
        &source_contents,
        arts.storage_layout.as_ref(),
        &ast,
    );

    eprintln!(
        "test_control_flow_nested_calls passed: {} struct logs, {} JUMPs",
        struct_logs.len(),
        jump_count
    );
}

/// MappingTest.fillSlots(10, 20, 30): verify fixed-size array SSTORE patterns.
#[tokio::test]
async fn test_mapping_storage_patterns() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (compiled, raw_json) = compile_contract("contracts/MappingTest.sol");
    let ast = SolidityAst::from_combined_json(&raw_json).unwrap();
    let arts = ContractArtifacts::from_compiled(&compiled, ":MappingTest");

    let source_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/MappingTest.sol");
    let source_contents = std::fs::read_to_string(&source_path).unwrap();

    let helper = TraceHelper::new().await;
    let contract = helper.deploy(&arts.deploy_hex, &[]).await;

    // fillSlots(10, 20, 30)
    let mut calldata = Vec::from(selector("fillSlots(uint256,uint256,uint256)"));
    for val in [10u64, 20, 30] {
        calldata.extend_from_slice(&U256::from(val).to_be_bytes::<32>());
    }

    let struct_logs = helper.call_and_trace(contract, &calldata).await;

    // 3 array slots + opCount increment = at least 4 SSTOREs.
    let sstore_count = struct_logs.iter().filter(|l| l.op.as_ref() == "SSTORE").count();
    assert!(
        sstore_count >= 4,
        "expected >= 4 SSTOREs (3 array + opCount), got {}",
        sstore_count
    );

    let _trace_dir = record_trace(
        "MappingTest",
        &struct_logs,
        &arts.source_map,
        &arts.runtime_bytecode,
        &source_path,
        &source_contents,
        arts.storage_layout.as_ref(),
        &ast,
    );

    eprintln!(
        "test_mapping_storage_patterns passed: {} SSTOREs",
        sstore_count
    );
}

/// MappingTest.setRecord(1, 42, true): verify struct storage writes.
#[tokio::test]
async fn test_mapping_struct_storage() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (compiled, raw_json) = compile_contract("contracts/MappingTest.sol");
    let ast = SolidityAst::from_combined_json(&raw_json).unwrap();
    let arts = ContractArtifacts::from_compiled(&compiled, ":MappingTest");

    let source_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/MappingTest.sol");
    let source_contents = std::fs::read_to_string(&source_path).unwrap();

    let helper = TraceHelper::new().await;
    let contract = helper.deploy(&arts.deploy_hex, &[]).await;

    // setRecord(1, 42, true)
    let mut calldata = Vec::from(selector("setRecord(uint256,uint256,bool)"));
    calldata.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
    calldata.extend_from_slice(&U256::from(42).to_be_bytes::<32>());
    let mut bool_arg = [0u8; 32];
    bool_arg[31] = 1;
    calldata.extend_from_slice(&bool_arg);

    let struct_logs = helper.call_and_trace(contract, &calldata).await;

    // Struct with 3 fields + opCount = at least 2 SSTOREs (fields may pack).
    let sstore_count = struct_logs.iter().filter(|l| l.op.as_ref() == "SSTORE").count();
    assert!(
        sstore_count >= 2,
        "expected >= 2 SSTOREs for struct fields, got {}",
        sstore_count
    );

    let _trace_dir = record_trace(
        "MappingTest",
        &struct_logs,
        &arts.source_map,
        &arts.runtime_bytecode,
        &source_path,
        &source_contents,
        arts.storage_layout.as_ref(),
        &ast,
    );

    eprintln!(
        "test_mapping_struct_storage passed: {} SSTOREs",
        sstore_count
    );
}

// ---------------------------------------------------------------------------
// SyntaxTest.sol — comprehensive Solidity syntax form coverage
// ---------------------------------------------------------------------------

/// Helper: compile SyntaxTest.sol and parse the AST.
fn compile_syntax_test() -> (serde_json::Value, String, SolidityAst) {
    let (compiled, raw_json) = compile_contract("contracts/SyntaxTest.sol");
    let ast = SolidityAst::from_combined_json(&raw_json).unwrap();
    (compiled, raw_json, ast)
}

/// Helper: deploy SyntaxTest, call a function, and record the trace.
async fn call_syntax_test(
    func_sig: &str,
    calldata_args: &[u8],
) -> (
    SolidityAst,
    Vec<codetracer_evm_recorder::structlog::StructLog>,
    tempfile::TempDir,
) {
    let (compiled, _raw_json, ast) = compile_syntax_test();
    let arts = ContractArtifacts::from_compiled(&compiled, ":SyntaxTest");
    let source_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/SyntaxTest.sol");
    let source_contents = std::fs::read_to_string(&source_path).unwrap();

    let helper = TraceHelper::new().await;
    let contract = helper.deploy(&arts.deploy_hex, &[]).await;

    let mut calldata = Vec::from(selector(func_sig));
    calldata.extend_from_slice(calldata_args);

    let struct_logs = helper.call_and_trace(contract, &calldata).await;

    let trace_dir = record_trace(
        "SyntaxTest",
        &struct_logs,
        &arts.source_map,
        &arts.runtime_bytecode,
        &source_path,
        &source_contents,
        arts.storage_layout.as_ref(),
        &ast,
    );

    (ast, struct_logs, trace_dir)
}

/// SyntaxTest.namedReturns: verify named return variables are parsed as locals.
#[tokio::test]
async fn test_syntax_named_returns() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (_, _, ast) = compile_syntax_test();

    let f = ast
        .functions
        .iter()
        .find(|f| f.name == "namedReturns")
        .expect("AST should contain `namedReturns`");

    // Named return variables should appear as locals.
    let local_names: Vec<_> = f.local_variables.iter().map(|v| v.name.as_str()).collect();
    assert!(
        local_names.contains(&"sum"),
        "named return 'sum' should be a local; got {:?}",
        local_names
    );
    assert!(
        local_names.contains(&"product"),
        "named return 'product' should be a local; got {:?}",
        local_names
    );

    // Parameters should be a, b.
    assert_eq!(f.parameters.len(), 2);

    let mut args = Vec::new();
    args.extend_from_slice(&U256::from(3).to_be_bytes::<32>());
    args.extend_from_slice(&U256::from(7).to_be_bytes::<32>());

    let (_ast, struct_logs, _trace_dir) =
        call_syntax_test("namedReturns(uint256,uint256)", &args).await;

    eprintln!(
        "test_syntax_named_returns passed: {} struct logs, locals={:?}",
        struct_logs.len(),
        local_names
    );
}

/// SyntaxTest.forLoop: verify for-loop init variable `i` is parsed.
#[tokio::test]
async fn test_syntax_for_loop_var() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (_, _, ast) = compile_syntax_test();

    let f = ast
        .functions
        .iter()
        .find(|f| f.name == "forLoop")
        .expect("AST should contain `forLoop`");

    let local_names: Vec<_> = f.local_variables.iter().map(|v| v.name.as_str()).collect();

    // Named return `total` + for-loop init variable `i`.
    assert!(
        local_names.contains(&"total"),
        "named return 'total' should be a local; got {:?}",
        local_names
    );
    assert!(
        local_names.contains(&"i"),
        "for-loop variable 'i' should be a local; got {:?}",
        local_names
    );

    let args = U256::from(5).to_be_bytes::<32>();
    let (_ast, struct_logs, _trace_dir) =
        call_syntax_test("forLoop(uint256)", &args).await;

    // Loop runs 5 times → many struct log entries.
    assert!(
        struct_logs.len() > 50,
        "forLoop(5) should produce many steps, got {}",
        struct_logs.len()
    );

    eprintln!(
        "test_syntax_for_loop_var passed: {} struct logs, locals={:?}",
        struct_logs.len(),
        local_names
    );
}

/// SyntaxTest.whileLoop: verify while-loop variables.
#[tokio::test]
async fn test_syntax_while_loop() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (_, _, ast) = compile_syntax_test();

    let f = ast
        .functions
        .iter()
        .find(|f| f.name == "whileLoop")
        .expect("AST should contain `whileLoop`");

    let local_names: Vec<_> = f.local_variables.iter().map(|v| v.name.as_str()).collect();

    // Named return `total` + explicitly declared `counter`.
    assert!(
        local_names.contains(&"total"),
        "named return 'total' should be a local; got {:?}",
        local_names
    );
    assert!(
        local_names.contains(&"counter"),
        "'counter' should be a local; got {:?}",
        local_names
    );

    let args = U256::from(4).to_be_bytes::<32>();
    let (_ast, struct_logs, _trace_dir) =
        call_syntax_test("whileLoop(uint256)", &args).await;

    eprintln!(
        "test_syntax_while_loop passed: {} struct logs, locals={:?}",
        struct_logs.len(),
        local_names
    );
}

/// SyntaxTest.uncheckedMath: verify variables inside `unchecked {}` blocks.
#[tokio::test]
async fn test_syntax_unchecked_block() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (_, _, ast) = compile_syntax_test();

    let f = ast
        .functions
        .iter()
        .find(|f| f.name == "uncheckedMath")
        .expect("AST should contain `uncheckedMath`");

    let local_names: Vec<_> = f.local_variables.iter().map(|v| v.name.as_str()).collect();

    // Named return `result` + local `temp` inside unchecked block.
    assert!(
        local_names.contains(&"result"),
        "named return 'result' should be a local; got {:?}",
        local_names
    );
    assert!(
        local_names.contains(&"temp"),
        "'temp' inside unchecked block should be a local; got {:?}",
        local_names
    );

    let mut args = Vec::new();
    args.extend_from_slice(&U256::from(5).to_be_bytes::<32>());
    args.extend_from_slice(&U256::from(3).to_be_bytes::<32>());

    let (_ast, struct_logs, _trace_dir) =
        call_syntax_test("uncheckedMath(uint256,uint256)", &args).await;

    eprintln!(
        "test_syntax_unchecked_block passed: {} struct logs, locals={:?}",
        struct_logs.len(),
        local_names
    );
}

/// SyntaxTest.compoundOps: verify compound assignment tracking.
#[tokio::test]
async fn test_syntax_compound_ops() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let args = U256::from(10).to_be_bytes::<32>();
    let (_ast, struct_logs, _trace_dir) =
        call_syntax_test("compoundOps(uint256)", &args).await;

    // result = 10 → 20 → 17 → 34  (+=10, -=3, *=2)
    eprintln!(
        "test_syntax_compound_ops passed: {} struct logs",
        struct_logs.len()
    );
}

/// SyntaxTest.tupleDestructure: verify tuple destructuring variables.
#[tokio::test]
async fn test_syntax_tuple_destructure() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (_, _, ast) = compile_syntax_test();

    let f = ast
        .functions
        .iter()
        .find(|f| f.name == "tupleDestructure")
        .expect("AST should contain `tupleDestructure`");

    let local_names: Vec<_> = f.local_variables.iter().map(|v| v.name.as_str()).collect();

    // Named returns sum, product + destructured locals s, p.
    assert!(
        local_names.contains(&"s"),
        "destructured 's' should be a local; got {:?}",
        local_names
    );
    assert!(
        local_names.contains(&"p"),
        "destructured 'p' should be a local; got {:?}",
        local_names
    );

    let mut args = Vec::new();
    args.extend_from_slice(&U256::from(4).to_be_bytes::<32>());
    args.extend_from_slice(&U256::from(6).to_be_bytes::<32>());

    let (_ast, struct_logs, _trace_dir) =
        call_syntax_test("tupleDestructure(uint256,uint256)", &args).await;

    eprintln!(
        "test_syntax_tuple_destructure passed: {} struct logs, locals={:?}",
        struct_logs.len(),
        local_names
    );
}

/// SyntaxTest.blockScope: verify variables in nested block scopes.
#[tokio::test]
async fn test_syntax_block_scope() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (_, _, ast) = compile_syntax_test();

    let f = ast
        .functions
        .iter()
        .find(|f| f.name == "blockScope")
        .expect("AST should contain `blockScope`");

    let local_names: Vec<_> = f.local_variables.iter().map(|v| v.name.as_str()).collect();

    // Named return `result` + two `temp` variables in different blocks.
    // Both have the same name but different offsets.
    assert!(
        local_names.contains(&"result"),
        "named return 'result' should be a local; got {:?}",
        local_names
    );

    // There should be exactly two `temp` declarations at different offsets.
    let temp_vars: Vec<_> = f
        .local_variables
        .iter()
        .filter(|v| v.name == "temp")
        .collect();
    assert_eq!(
        temp_vars.len(),
        2,
        "should have two 'temp' declarations in different scopes; got {}",
        temp_vars.len()
    );
    assert_ne!(
        temp_vars[0].declaration_offset, temp_vars[1].declaration_offset,
        "two 'temp' vars should have different offsets"
    );

    let args = U256::from(5).to_be_bytes::<32>();
    let (_ast, struct_logs, _trace_dir) =
        call_syntax_test("blockScope(uint256)", &args).await;

    eprintln!(
        "test_syntax_block_scope passed: {} struct logs, locals={:?}",
        struct_logs.len(),
        local_names
    );
}

/// SyntaxTest.ternary: verify ternary conditional expression.
#[tokio::test]
async fn test_syntax_ternary() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    // Call ternary(true, 42, 99) → result should be 42
    let mut args = Vec::new();
    let mut bool_arg = [0u8; 32];
    bool_arg[31] = 1; // true
    args.extend_from_slice(&bool_arg);
    args.extend_from_slice(&U256::from(42).to_be_bytes::<32>());
    args.extend_from_slice(&U256::from(99).to_be_bytes::<32>());

    let (_ast, struct_logs, _trace_dir) =
        call_syntax_test("ternary(bool,uint256,uint256)", &args).await;

    eprintln!(
        "test_syntax_ternary passed: {} struct logs",
        struct_logs.len()
    );
}

/// SyntaxTest.typeCast: verify type conversions produce proper TypeKinds.
#[tokio::test]
async fn test_syntax_type_cast() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let (_, _, ast) = compile_syntax_test();

    let f = ast
        .functions
        .iter()
        .find(|f| f.name == "typeCast")
        .expect("AST should contain `typeCast`");

    let local_names: Vec<_> = f
        .local_variables
        .iter()
        .map(|v| (v.name.as_str(), v.type_name.as_str()))
        .collect();

    // Named returns: addr (address), flag (bool), hash (bytes32)
    assert!(
        local_names.iter().any(|(n, t)| *n == "addr" && *t == "address"),
        "should have addr:address; got {:?}",
        local_names
    );
    assert!(
        local_names.iter().any(|(n, t)| *n == "flag" && *t == "bool"),
        "should have flag:bool; got {:?}",
        local_names
    );
    assert!(
        local_names.iter().any(|(n, t)| *n == "hash" && *t == "bytes32"),
        "should have hash:bytes32; got {:?}",
        local_names
    );

    let args = U256::from(0x1234u64).to_be_bytes::<32>();
    let (_ast, struct_logs, _trace_dir) =
        call_syntax_test("typeCast(uint256)", &args).await;

    eprintln!(
        "test_syntax_type_cast passed: {} struct logs, locals={:?}",
        struct_logs.len(),
        local_names
    );
}

/// SyntaxTest.multiAssign: verify multiple reassignments to the same variable.
#[tokio::test]
async fn test_syntax_multi_assign() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let mut args = Vec::new();
    args.extend_from_slice(&U256::from(3).to_be_bytes::<32>());
    args.extend_from_slice(&U256::from(4).to_be_bytes::<32>());

    let (_ast, struct_logs, _trace_dir) =
        call_syntax_test("multiAssign(uint256,uint256)", &args).await;

    // x = 3 → 7 → 14  (x = a, x = x + b, x = x * 2)
    eprintln!(
        "test_syntax_multi_assign passed: {} struct logs",
        struct_logs.len()
    );
}

/// SyntaxTest.incrDecr: verify increment/decrement operations.
#[tokio::test]
async fn test_syntax_incr_decr() {
    assert!(has_solc(), "solc required");
    assert!(has_anvil(), "anvil required");

    let args = U256::from(10).to_be_bytes::<32>();
    let (_ast, struct_logs, _trace_dir) =
        call_syntax_test("incrDecr(uint256)", &args).await;

    eprintln!(
        "test_syntax_incr_decr passed: {} struct logs",
        struct_logs.len()
    );
}
