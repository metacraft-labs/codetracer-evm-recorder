//! Diagnostic test: traces the FlowTest contract and dumps what the
//! stack tracker + AST see at each step.  Not a pass/fail test — prints
//! diagnostic information to help debug variable tracking issues.
//!
//! Run with: `cargo test --test test_var_tracking_diagnostic -- --nocapture`

use std::process::Command;

use codetracer_evm_recorder::solidity_ast::SolidityAst;
use codetracer_evm_recorder::source_map::{self, SourceMap};
use codetracer_evm_recorder::stack_tracker::StackTracker;

fn compile_contract_with_ast() -> (serde_json::Value, String) {
    let output = Command::new("solc")
        .args([
            "--combined-json",
            "abi,bin,bin-runtime,srcmap-runtime,storage-layout,ast",
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
    let raw = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    (json, raw)
}

fn opcode_from_name(name: &str) -> Option<u8> {
    // Reuse the same mapping from recorder.rs
    match name {
        "STOP" => Some(0x00),
        "ADD" => Some(0x01),
        "MUL" => Some(0x02),
        "SUB" => Some(0x03),
        "DIV" => Some(0x04),
        "SDIV" => Some(0x05),
        "MOD" => Some(0x06),
        "SMOD" => Some(0x07),
        "ADDMOD" => Some(0x08),
        "MULMOD" => Some(0x09),
        "EXP" => Some(0x0a),
        "SIGNEXTEND" => Some(0x0b),
        "LT" => Some(0x10),
        "GT" => Some(0x11),
        "SLT" => Some(0x12),
        "SGT" => Some(0x13),
        "EQ" => Some(0x14),
        "ISZERO" => Some(0x15),
        "AND" => Some(0x16),
        "OR" => Some(0x17),
        "XOR" => Some(0x18),
        "NOT" => Some(0x19),
        "BYTE" => Some(0x1a),
        "SHL" => Some(0x1b),
        "SHR" => Some(0x1c),
        "SAR" => Some(0x1d),
        "SHA3" | "KECCAK256" => Some(0x20),
        "ADDRESS" => Some(0x30),
        "BALANCE" => Some(0x31),
        "ORIGIN" => Some(0x32),
        "CALLER" => Some(0x33),
        "CALLVALUE" => Some(0x34),
        "CALLDATALOAD" => Some(0x35),
        "CALLDATASIZE" => Some(0x36),
        "CALLDATACOPY" => Some(0x37),
        "CODESIZE" => Some(0x38),
        "CODECOPY" => Some(0x39),
        "GASPRICE" => Some(0x3a),
        "EXTCODESIZE" => Some(0x3b),
        "EXTCODECOPY" => Some(0x3c),
        "RETURNDATASIZE" => Some(0x3d),
        "RETURNDATACOPY" => Some(0x3e),
        "EXTCODEHASH" => Some(0x3f),
        "BLOCKHASH" => Some(0x40),
        "COINBASE" => Some(0x41),
        "TIMESTAMP" => Some(0x42),
        "NUMBER" => Some(0x43),
        "PREVRANDAO" | "DIFFICULTY" => Some(0x44),
        "GASLIMIT" => Some(0x45),
        "CHAINID" => Some(0x46),
        "SELFBALANCE" => Some(0x47),
        "BASEFEE" => Some(0x48),
        "BLOBHASH" => Some(0x49),
        "BLOBBASEFEE" => Some(0x4a),
        "POP" => Some(0x50),
        "MLOAD" => Some(0x51),
        "MSTORE" => Some(0x52),
        "MSTORE8" => Some(0x53),
        "SLOAD" => Some(0x54),
        "SSTORE" => Some(0x55),
        "JUMP" => Some(0x56),
        "JUMPI" => Some(0x57),
        "PC" => Some(0x58),
        "MSIZE" => Some(0x59),
        "GAS" => Some(0x5a),
        "JUMPDEST" => Some(0x5b),
        "TLOAD" => Some(0x5c),
        "TSTORE" => Some(0x5d),
        "MCOPY" => Some(0x5e),
        "PUSH0" => Some(0x5f),
        "RETURN" => Some(0xf3),
        "REVERT" => Some(0xfd),
        "SELFDESTRUCT" => Some(0xff),
        "INVALID" => Some(0xfe),
        "CREATE" => Some(0xf0),
        "CALL" => Some(0xf1),
        "CALLCODE" => Some(0xf2),
        "DELEGATECALL" => Some(0xf4),
        "CREATE2" => Some(0xf5),
        "STATICCALL" => Some(0xfa),
        _ => {
            // PUSHn, DUPn, SWAPn, LOGn
            if let Some(rest) = name.strip_prefix("PUSH") {
                let n: u8 = rest.parse().ok()?;
                Some(0x5f + n) // PUSH1=0x60, PUSH2=0x61, ...
            } else if let Some(rest) = name.strip_prefix("DUP") {
                let n: u8 = rest.parse().ok()?;
                Some(0x7f + n) // DUP1=0x80
            } else if let Some(rest) = name.strip_prefix("SWAP") {
                let n: u8 = rest.parse().ok()?;
                Some(0x8f + n) // SWAP1=0x90
            } else if let Some(rest) = name.strip_prefix("LOG") {
                let n: u8 = rest.parse().ok()?;
                Some(0xa0 + n)
            } else {
                None
            }
        }
    }
}

#[tokio::test]
async fn test_var_tracking_diagnostic() {
    assert!(
        Command::new("solc")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    );
    assert!(
        Command::new("anvil")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    );

    let (compiled, raw_json) = compile_contract_with_ast();

    // Parse AST
    let ast = SolidityAst::from_combined_json(&raw_json).unwrap();
    eprintln!("\n=== AST Functions ===");
    for f in &ast.functions {
        eprintln!(
            "  {} (src: {}:{} file:{})",
            f.name, f.src.offset, f.src.length, f.src.file_index
        );
        for p in &f.parameters {
            eprintln!(
                "    param: {} (decl_off={}, src={}:{})",
                p.name, p.declaration_offset, p.src.offset, p.src.length
            );
        }
        for lv in &f.local_variables {
            let stmt = lv
                .statement_range
                .as_ref()
                .map(|r| format!("{}:{}", r.offset, r.length))
                .unwrap_or_else(|| "none".into());
            eprintln!(
                "    local: {} (decl_off={}, src={}:{}, stmt={})",
                lv.name, lv.declaration_offset, lv.src.offset, lv.src.length, stmt
            );
        }
    }

    // Get source
    let source_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/FlowTest.sol");
    let source_contents = std::fs::read_to_string(&source_path).unwrap();

    // Get contract
    let contracts = compiled["contracts"].as_object().unwrap();
    let (_, contract_json) = contracts
        .iter()
        .find(|(k, _)| k.ends_with(":FlowTest"))
        .unwrap();
    let runtime_bytecode_hex = contract_json["bin-runtime"].as_str().unwrap();
    let source_map_raw = contract_json["srcmap-runtime"].as_str().unwrap();
    let deploy_bytecode_hex = contract_json["bin"].as_str().unwrap();

    let source_map = SourceMap::parse(source_map_raw);
    let runtime_bytecode = alloy::hex::decode(runtime_bytecode_hex).unwrap();
    let pc_to_idx = source_map::build_pc_to_instruction_index(&runtime_bytecode);

    // Deploy and call run()
    let anvil = alloy::node_bindings::Anvil::new()
        .arg("--steps-tracing")
        .spawn();
    let rpc_url = anvil.endpoint();
    let provider = alloy::providers::ProviderBuilder::new().connect_http(rpc_url.parse().unwrap());
    use alloy::providers::Provider;
    let accounts = provider.get_accounts().await.unwrap();
    let from = accounts[0];

    let deploy_bytes = alloy::hex::decode(deploy_bytecode_hex).unwrap();

    use alloy::network::TransactionBuilder;
    let deploy_tx = alloy::rpc::types::TransactionRequest::default()
        .from(from)
        .with_deploy_code(alloy::primitives::Bytes::from(deploy_bytes));
    let deploy_receipt = provider
        .send_transaction(deploy_tx)
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();
    let contract_address = deploy_receipt.contract_address.unwrap();

    // Call compute()
    let selector = &alloy::primitives::keccak256("compute()".as_bytes())[..4];
    let call_tx = alloy::rpc::types::TransactionRequest::default()
        .from(from)
        .to(contract_address)
        .with_input(alloy::primitives::Bytes::copy_from_slice(selector));
    let call_receipt = provider
        .send_transaction(call_tx)
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();
    let tx_hash = call_receipt.transaction_hash;

    // Trace
    let frame = codetracer_evm_recorder::trace_fetcher::fetch_struct_logs(&rpc_url, tx_hash)
        .await
        .unwrap();
    let struct_logs = codetracer_evm_recorder::trace_fetcher::extract_struct_logs(&frame);

    eprintln!("\n=== Struct Logs ({} entries) ===", struct_logs.len());

    let mut tracker = StackTracker::new();
    let mut prev_line: Option<(i32, u32)> = None;

    for (i, log) in struct_logs.iter().enumerate() {
        let pc = log.pc as usize;
        let opcode = opcode_from_name(log.op.as_ref());

        let source_offset = source_map
            .get_entry_for_pc(pc, &pc_to_idx)
            .filter(|e| e.file_index >= 0)
            .map(|e| e.offset);

        if let Some(location) = source_map.resolve_pc(pc, &pc_to_idx, &[source_contents.as_str()]) {
            let current = (location.file_index, location.line);

            // Only print steps within function bodies (lines 10-23)
            if location.line >= 10 && location.line <= 23 {
                if prev_line != Some(current) {
                    eprintln!("\n--- Line {} ---", location.line);
                }

                let func = ast.function_at(source_offset.unwrap_or(-1), location.file_index);
                let in_scope: Vec<_> = func
                    .map(|f| f.vars_in_scope_at(source_offset.unwrap_or(-1)))
                    .unwrap_or_default();
                let in_scope_names: Vec<_> = in_scope.iter().map(|v| v.name.as_str()).collect();

                if let Some(op) = opcode {
                    let assignments = tracker.process_step(op, pc, source_offset, &in_scope);

                    let post_stack = struct_logs.get(i + 1).and_then(|n| n.stack.as_ref());
                    let mut var_vals: Vec<(String, String)> = Vec::new();
                    if let Some(concrete) = post_stack {
                        for var in &in_scope {
                            if let Some(val) = tracker.get_variable_value(&var.name, concrete) {
                                var_vals.push((var.name.clone(), format!("{}", val)));
                            }
                        }
                    }

                    let stack_depth = log.stack.as_ref().map(|s| s.len()).unwrap_or(0);
                    eprintln!(
                        "  [{:3}] pc={:4} {:12} src_off={:>5} stack_depth={:2} tracker_depth={:2} in_scope={:?} assignments={:?} vars={:?}",
                        i,
                        pc,
                        log.op.as_ref(),
                        source_offset
                            .map(|o| o.to_string())
                            .unwrap_or_else(|| "-".into()),
                        stack_depth,
                        tracker.depth(),
                        in_scope_names,
                        assignments
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>(),
                        var_vals,
                    );
                }
            } else {
                // Outside run(), still advance tracker
                if let Some(op) = opcode {
                    let _ = tracker.process_step(op, pc, source_offset, &[]);
                }
            }
            prev_line = Some(current);
        } else if let Some(op) = opcode {
            let _ = tracker.process_step(op, pc, source_offset, &[]);
        }
    }

    eprintln!("\n=== Done ===\n");
}
