//! End-to-end integration test: compile, deploy, trace, and verify.
//!
//! Requires `solc` and `anvil` to be available on PATH.
//! Requires `solc` and `anvil` on PATH (provided by the Nix dev shell).

use std::process::Command;

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

/// Compile FlowTest.sol with solc, returning the combined JSON output.
fn compile_contract() -> serde_json::Value {
    let output = Command::new("solc")
        .args([
            "--combined-json",
            "abi,bin,bin-runtime,srcmap-runtime,storage-layout",
            "--no-cbor-metadata",
            "contracts/FlowTest.sol",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run solc");
    assert!(
        output.status.success(),
        "solc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("solc output is not valid JSON")
}

#[tokio::test]
async fn test_e2e_trace() {
    assert!(
        has_solc(),
        "solc must be available on PATH to run this test (use `nix develop` or install solc)"
    );
    assert!(
        has_anvil(),
        "anvil must be available on PATH to run this test (use `nix develop` or install foundry)"
    );

    // 1. Compile the contract
    let compiled = compile_contract();
    let contracts = compiled["contracts"].as_object().unwrap();

    // Find the FlowTest contract (key format: "contracts/FlowTest.sol:FlowTest")
    let (_, contract_json) = contracts
        .iter()
        .find(|(k, _)| k.ends_with(":FlowTest"))
        .expect("FlowTest not found in solc output");

    let deploy_bytecode_hex = contract_json["bin"].as_str().expect("missing bin");
    let runtime_bytecode_hex = contract_json["bin-runtime"]
        .as_str()
        .expect("missing bin-runtime");
    let source_map_raw = contract_json["srcmap-runtime"]
        .as_str()
        .expect("missing srcmap-runtime");
    let storage_layout_json = &contract_json["storage-layout"];

    // Parse source map and storage layout
    let source_map = codetracer_evm_recorder::source_map::SourceMap::parse(source_map_raw);
    assert!(!source_map.is_empty(), "source map should not be empty");

    let storage_layout: codetracer_evm_recorder::storage_layout::StorageLayout =
        serde_json::from_value(storage_layout_json.clone())
            .expect("failed to parse storage layout");
    assert_eq!(storage_layout.storage.len(), 2);

    let runtime_bytecode =
        alloy::hex::decode(runtime_bytecode_hex).expect("invalid runtime bytecode hex");

    // Read the source file
    let source_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/FlowTest.sol");
    let source_contents =
        std::fs::read_to_string(&source_path).expect("failed to read FlowTest.sol");

    // 2. Spawn anvil
    // `--steps-tracing` is required for debug_traceTransaction to return non-empty
    // structLogs in foundry/anvil >= 1.5.0. Without it, structLogs is always empty.
    let anvil = alloy::node_bindings::Anvil::new()
        .arg("--steps-tracing")
        .spawn();
    let rpc_url = anvil.endpoint();

    // 3. Deploy the contract
    let provider = alloy::providers::ProviderBuilder::new().connect_http(rpc_url.parse().unwrap());

    use alloy::providers::Provider;
    let accounts = provider.get_accounts().await.unwrap();
    let from = accounts[0];

    let deploy_data = alloy::primitives::Bytes::from(
        alloy::hex::decode(deploy_bytecode_hex).expect("invalid deploy bytecode hex"),
    );

    use alloy::network::TransactionBuilder;
    let deploy_tx = alloy::rpc::types::TransactionRequest::default()
        .from(from)
        .with_deploy_code(deploy_data);

    let deploy_pending = provider.send_transaction(deploy_tx).await.unwrap();
    let deploy_receipt = deploy_pending.get_receipt().await.unwrap();
    let contract_address = deploy_receipt
        .contract_address
        .expect("no contract address in deploy receipt");

    // 4. Call compute()
    // Selector for compute() is keccak256("compute()")[:4]
    let selector = &alloy::primitives::keccak256("compute()".as_bytes())[..4];
    let call_tx = alloy::rpc::types::TransactionRequest::default()
        .from(from)
        .to(contract_address)
        .with_input(alloy::primitives::Bytes::copy_from_slice(selector));

    let call_pending = provider.send_transaction(call_tx).await.unwrap();
    let call_receipt = call_pending.get_receipt().await.unwrap();
    let tx_hash = call_receipt.transaction_hash;

    // 5. Fetch debug_traceTransaction
    let frame = codetracer_evm_recorder::trace_fetcher::fetch_struct_logs(&rpc_url, tx_hash)
        .await
        .expect("fetch_struct_logs failed");

    let struct_logs = codetracer_evm_recorder::trace_fetcher::extract_struct_logs(&frame);
    assert!(
        !struct_logs.is_empty(),
        "structLog should not be empty for compute() call"
    );

    // 6. Process through recorder
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let mut recorder =
        codetracer_evm_recorder::recorder::EvmRecorder::new("FlowTest", tmp_dir.path()).unwrap();
    recorder.initialize().unwrap();

    let source_path_ref: &std::path::Path = source_path.as_path();
    let source_contents_ref: &str = &source_contents;

    recorder
        .record_from_structlog(
            struct_logs,
            &source_map,
            &runtime_bytecode,
            &[source_path_ref],
            &[source_contents_ref],
            Some(&storage_layout),
            None, // no AST for this e2e test
        )
        .unwrap();

    recorder.finalize().unwrap();

    // 7. Verify output files exist
    assert!(tmp_dir.path().join("trace.bin").exists());
    assert!(tmp_dir.path().join("trace_metadata.json").exists());
    assert!(tmp_dir.path().join("trace_paths.json").exists());

    // 8. Verify we had SSTORE opcodes (storage writes)
    let sstore_count = struct_logs
        .iter()
        .filter(|l| l.op.as_ref() == "SSTORE")
        .count();
    assert!(
        sstore_count >= 2,
        "expected at least 2 SSTORE operations (storedA and storedResult), got {}",
        sstore_count
    );

    // 9. Verify we had LOG opcodes (Solidity events)
    let log_count = struct_logs
        .iter()
        .filter(|l| l.op.as_ref().starts_with("LOG"))
        .count();
    assert!(
        log_count >= 1,
        "expected at least 1 LOG operation (Computed event), got {}",
        log_count
    );

    eprintln!(
        "E2E test passed: {} struct log entries, {} SSTOREs, {} LOGs",
        struct_logs.len(),
        sstore_count,
        log_count
    );
}
