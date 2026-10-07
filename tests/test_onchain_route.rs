//! Integration tests for the on-chain recording route (`onchain.rs`, the
//! `trace-onchain` subcommand).
//!
//! # Why anvil and not a public endpoint
//!
//! The route's contract is "an endpoint that serves ARCHIVE state at the
//! target block's parent, and no tracer at all".  A local anvil node meets
//! that contract exactly — it keeps full history for its own chain and
//! `trace-onchain` never asks it for `debug_*` — so the whole route,
//! including the `replay-preceding` prestate reconstruction, is exercised
//! here with no public endpoint and no flakiness.
//!
//! What anvil cannot stand in for is Sourcify verification, so these tests
//! run with `skip_source_fetch` and the opcode-granularity path.  Source
//! recovery is covered by the unit tests in `onchain.rs` (path
//! confinement, the bytecode-prefix rule, solc selection) and by the
//! recorded mainnet run documented in the commit that adds this file.
//!
//! No mock objects are used: the EVM is real revm, the chain is a real
//! anvil node over real HTTP, the container is written by the real Nim
//! CTFS writer and read back through the real Nim reader FFI.
//!
//! Requires `anvil` on PATH (provided by the Nix dev shell).

use std::path::Path;
use std::process::Command;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::TxHash;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;

use codetracer_evm_recorder::onchain::{OnchainOptions, record_onchain_transaction};
use codetracer_trace_writer_nim::NimTraceReaderHandle;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn has_anvil() -> bool {
    Command::new("anvil")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Init code for a contract whose runtime SSTOREs 0xDEAD into slot 0,
/// MSTOREs it, emits LOG0 over those 32 bytes, and STOPs.
///
/// Hand-assembled rather than compiled so the test has no solc dependency
/// and the expected opcode sequence is fixed by the test itself.
fn simple_contract_init_code() -> alloy::primitives::Bytes {
    use revm::state::bytecode::opcode;

    let runtime: Vec<u8> = vec![
        opcode::PUSH2,
        0xDE,
        0xAD,
        opcode::PUSH1,
        0x00,
        opcode::SSTORE,
        opcode::PUSH2,
        0xDE,
        0xAD,
        opcode::PUSH1,
        0x00,
        opcode::MSTORE,
        opcode::PUSH1,
        0x20,
        opcode::PUSH1,
        0x00,
        opcode::LOG0,
        opcode::STOP,
    ];
    let runtime_len = runtime.len() as u8;
    let init_code_len = 12u8;

    let mut init: Vec<u8> = vec![
        opcode::PUSH1,
        runtime_len,
        opcode::PUSH1,
        init_code_len,
        opcode::PUSH1,
        0x00,
        opcode::CODECOPY,
        opcode::PUSH1,
        runtime_len,
        opcode::PUSH1,
        0x00,
        opcode::RETURN,
    ];
    assert_eq!(
        init.len(),
        init_code_len as usize,
        "init code length mismatch"
    );
    init.extend_from_slice(&runtime);
    alloy::primitives::Bytes::from(init)
}

/// Locate the single `.ct` container the recorder produced in `dir`.
fn find_ct_container(dir: &Path) -> std::path::PathBuf {
    std::fs::read_dir(dir)
        .expect("trace dir unreadable")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "ct"))
        .expect("no .ct container in trace dir")
}

// ---------------------------------------------------------------------------
// The route, end to end
// ---------------------------------------------------------------------------

/// The join this milestone added: a transaction that is already on a chain
/// reaches a CTFS container that the CURRENT Nim reader opens.
#[tokio::test(flavor = "multi_thread")]
async fn trace_onchain_writes_a_container_the_nim_reader_opens() {
    if !has_anvil() {
        panic!(
            "anvil is not on PATH — this test asserts a real chain round trip and \
             must not be silently skipped; run it inside `nix develop`"
        );
    }

    use alloy::node_bindings::Anvil;
    let anvil = Anvil::new().spawn();
    let rpc_url = anvil.endpoint();

    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let deployer = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(rpc_url.parse().unwrap());

    let deploy_receipt = provider
        .send_transaction(
            TransactionRequest::default()
                .from(deployer)
                .with_deploy_code(simple_contract_init_code()),
        )
        .await
        .expect("send deploy tx")
        .get_receipt()
        .await
        .expect("deploy receipt");
    assert!(deploy_receipt.status(), "deployment should succeed");
    let contract = deploy_receipt
        .contract_address
        .expect("deployment returns an address");

    let call_receipt = provider
        .send_transaction(
            TransactionRequest::default()
                .from(deployer)
                .to(contract)
                .with_input(alloy::primitives::Bytes::new()),
        )
        .await
        .expect("send call tx")
        .get_receipt()
        .await
        .expect("call receipt");
    assert!(call_receipt.status(), "call should succeed");
    let tx_hash: TxHash = call_receipt.transaction_hash;

    let tmp = tempfile::TempDir::new().unwrap();
    let mut options = OnchainOptions::new(rpc_url.clone(), tx_hash, tmp.path());
    // anvil has no Sourcify presence; source recovery is covered elsewhere.
    options.skip_source_fetch = true;

    let recording = record_onchain_transaction(&options)
        .await
        .expect("the on-chain route should record an anvil transaction");

    // The chain context the route reports must be the chain it ran against.
    let chain_id = provider.get_chain_id().await.unwrap();
    assert_eq!(recording.chain_id, chain_id);
    assert_eq!(
        recording.block_number,
        call_receipt.block_number.unwrap(),
        "the recording must name the block the transaction was mined in"
    );
    assert_eq!(recording.entry_point, contract);
    assert!(recording.succeeded, "the replayed call should succeed");
    assert!(recording.memory_captured, "memory capture is on by default");

    // The hand-assembled runtime is 18 bytes of 10 opcodes; the replay sees
    // every one of them.
    assert_eq!(
        recording.step_count, 10,
        "the 10-opcode runtime should produce exactly 10 steps"
    );
    assert_eq!(recording.log_count, 1, "the runtime emits exactly one LOG0");

    // ---- the container, through the current reader ----------------------
    // The route reopens its own output, so these assertions pin the
    // reported figures against the file on disk rather than repeating the
    // route's internal check.
    let ct_path = find_ct_container(tmp.path());
    assert_eq!(
        recording.container_path, ct_path,
        "the route must name the container it actually wrote"
    );
    assert_eq!(
        recording.container_bytes,
        std::fs::metadata(&ct_path).unwrap().len()
    );
    assert!(
        recording.container_bytes > 0,
        "the container must not be empty"
    );
    assert!(
        recording.container_steps_read_back > 0,
        "the route must have read steps back out of its own container"
    );

    let reader = NimTraceReaderHandle::open(ct_path.to_str().unwrap())
        .unwrap_or_else(|e| panic!("the current Nim reader failed to open {ct_path:?}: {e}"));
    assert_eq!(
        reader.step_count(),
        recording.container_steps_read_back,
        "an independent open must agree with the route's own read-back"
    );

    // Container version is byte 5 of the header (`ctfs-container.md` §1:
    // magic[0..5], version[5], encryption[6], max_shards[7]).  Pinned
    // because the workspace has just reconciled versions: the Nim reader
    // takes container 5 (and 6), and the db-backend takes {2,3,4,5}, so 5
    // is the version both accept.
    let header = std::fs::read(&ct_path).unwrap();
    assert_eq!(&header[..5], b"\xc0\xder\xac\xe2", "CTFS magic");
    assert_eq!(header[5], 5, "the route must emit a version-5 container");

    // `meta.dat` schema version is bytes 4..6 of the `CTMD` record, little
    // endian.  Version 6 is what the current writer writes and the only
    // one it reads back.
    let ctmd = header
        .windows(4)
        .position(|w| w == b"CTMD")
        .expect("the container must carry a meta.dat record");
    let meta_version = u16::from_le_bytes([header[ctmd + 4], header[ctmd + 5]]);
    assert_eq!(meta_version, 6, "meta.dat schema version");
}

/// The declared prestate strategy is `replay-preceding`, and this asserts
/// the producer actually does it: with automine off, two transactions land
/// in ONE block, and tracing the second one replays the first to rebuild
/// its prestate.
///
/// CONTROL: the first transaction's own recording replays ZERO predecessors
/// from the same block, so the arm distinguishes "replayed the block's
/// earlier transactions" from "always reports the index".
#[tokio::test(flavor = "multi_thread")]
async fn the_preceding_transactions_in_the_block_are_replayed() {
    if !has_anvil() {
        panic!(
            "anvil is not on PATH — this test asserts a real chain round trip and \
             must not be silently skipped; run it inside `nix develop`"
        );
    }

    use alloy::node_bindings::Anvil;
    let anvil = Anvil::new().spawn();
    let rpc_url = anvil.endpoint();

    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let deployer = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(rpc_url.parse().unwrap());

    let deploy_receipt = provider
        .send_transaction(
            TransactionRequest::default()
                .from(deployer)
                .with_deploy_code(simple_contract_init_code()),
        )
        .await
        .expect("send deploy tx")
        .get_receipt()
        .await
        .expect("deploy receipt");
    let contract = deploy_receipt.contract_address.unwrap();

    // Hold both calls back so they share a block.
    // `evm_setAutomine` / `evm_mine` answer with JSON null, so the response
    // type has to be `Value` rather than a typed result.
    provider
        .raw_request::<Vec<serde_json::Value>, serde_json::Value>(
            "evm_setAutomine".into(),
            vec![serde_json::Value::Bool(false)],
        )
        .await
        .expect("anvil should accept evm_setAutomine");

    let mut pending = Vec::new();
    for _ in 0..2 {
        pending.push(
            provider
                .send_transaction(
                    TransactionRequest::default()
                        .from(deployer)
                        .to(contract)
                        .with_input(alloy::primitives::Bytes::new())
                        .gas_limit(100_000),
                )
                .await
                .expect("send call tx"),
        );
    }

    provider
        .raw_request::<Vec<serde_json::Value>, serde_json::Value>("evm_mine".into(), vec![])
        .await
        .expect("anvil should accept evm_mine");

    let mut hashes: Vec<TxHash> = Vec::new();
    for p in pending {
        let receipt = p.get_receipt().await.expect("call receipt");
        assert!(receipt.status(), "both calls should succeed");
        hashes.push(receipt.transaction_hash);
    }

    // Read the block back so the assertion is about the chain's own
    // ordering rather than the order the test submitted in.
    let block_number = provider
        .get_transaction_by_hash(hashes[0])
        .await
        .unwrap()
        .unwrap()
        .block_number
        .unwrap();
    let block = provider
        .get_block_by_number(block_number.into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        block.transactions.len(),
        2,
        "the two calls must share one block for this arm to mean anything"
    );

    let mut indexed: Vec<(u64, TxHash)> = Vec::new();
    for h in &hashes {
        let tx = provider.get_transaction_by_hash(*h).await.unwrap().unwrap();
        indexed.push((tx.transaction_index.unwrap(), *h));
    }
    indexed.sort();

    // CONTROL: index 0 has no predecessors in its block.
    let tmp_first = tempfile::TempDir::new().unwrap();
    let mut first_opts = OnchainOptions::new(rpc_url.clone(), indexed[0].1, tmp_first.path());
    first_opts.skip_source_fetch = true;
    let first = record_onchain_transaction(&first_opts)
        .await
        .expect("the index-0 transaction should record");
    assert_eq!(first.tx_index, 0);
    assert_eq!(
        first.preceding_replayed, 0,
        "nothing precedes the first transaction in its own block"
    );

    // The arm: index 1 replays exactly its one predecessor.
    let tmp_second = tempfile::TempDir::new().unwrap();
    let mut second_opts = OnchainOptions::new(rpc_url.clone(), indexed[1].1, tmp_second.path());
    second_opts.skip_source_fetch = true;
    let second = record_onchain_transaction(&second_opts)
        .await
        .expect("the index-1 transaction should record");
    assert_eq!(second.tx_index, 1);
    assert_eq!(
        second.preceding_replayed, 1,
        "the transaction at index 1 must have its one predecessor replayed"
    );

    // Both containers must open.
    for dir in [tmp_first.path(), tmp_second.path()] {
        let ct = find_ct_container(dir);
        NimTraceReaderHandle::open(ct.to_str().unwrap())
            .unwrap_or_else(|e| panic!("the current Nim reader failed to open {ct:?}: {e}"));
    }
}

// ---------------------------------------------------------------------------
// CLI surface
// ---------------------------------------------------------------------------

/// `trace-onchain` must refuse rather than guess when no endpoint is given,
/// and the refusal must name the environment variable that would supply one.
#[test]
fn trace_onchain_without_an_endpoint_fails_and_names_the_env_var() {
    let bin = env!("CARGO_BIN_EXE_codetracer-evm-recorder");
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(bin)
        .args([
            "trace-onchain",
            "0x0000000000000000000000000000000000000000000000000000000000000001",
            "--out-dir",
        ])
        .arg(tmp.path())
        .env_remove("CODETRACER_EVM_RECORDER_RPC_URL")
        .env_remove("CODETRACER_EVM_RECORDER_DISABLED")
        .output()
        .expect("failed to run trace-onchain");

    assert!(
        !output.status.success(),
        "trace-onchain must fail without an endpoint; it exited 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("CODETRACER_EVM_RECORDER_RPC_URL"),
        "the refusal must name the env var that supplies an endpoint; got:\n{stderr}"
    );
}

/// A malformed transaction hash is rejected before any endpoint is
/// contacted, and the message says what was wrong with it.
#[test]
fn trace_onchain_rejects_a_malformed_transaction_hash() {
    let bin = env!("CARGO_BIN_EXE_codetracer-evm-recorder");
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(bin)
        .args([
            "trace-onchain",
            "not-a-hash",
            "--rpc-url",
            "http://127.0.0.1:1",
            "--out-dir",
        ])
        .arg(tmp.path())
        .output()
        .expect("failed to run trace-onchain");

    assert!(!output.status.success(), "a malformed hash must not exit 0");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("transaction hash"),
        "the refusal must say the hash is the problem; got:\n{stderr}"
    );
}

/// `CODETRACER_EVM_RECORDER_DISABLED=1` makes `trace-onchain` a
/// pass-through: exit 0, no endpoint contacted, no artefacts written.
/// Convention: `Recorder-CLI-Conventions.md` §5.
#[test]
fn trace_onchain_honours_the_disabled_env_var() {
    let bin = env!("CARGO_BIN_EXE_codetracer-evm-recorder");
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(bin)
        .args([
            "trace-onchain",
            "0x0000000000000000000000000000000000000000000000000000000000000001",
            // Deliberately unroutable: if the binary contacted it, the run
            // would not come back clean.
            "--rpc-url",
            "http://127.0.0.1:1",
            "--out-dir",
        ])
        .arg(tmp.path())
        .env("CODETRACER_EVM_RECORDER_DISABLED", "1")
        .output()
        .expect("failed to run trace-onchain");

    assert!(
        output.status.success(),
        "disabled mode must exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(
        entries.is_empty(),
        "disabled mode must write nothing; found {entries:?}"
    );
}
