//! End-to-end replay test that requires a running Anvil instance.
//!
//! This test is marked `#[ignore]` because it requires:
//!   - `anvil` to be in PATH (from foundry)
//!   - Network access (local loopback)
//!
//! Run with: `cargo test test_replay_anvil -- --ignored`

use alloy::primitives::TxHash;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::rpc::types::TransactionRequest;
use codetracer_evm_recorder::replay::replay_transaction;

/// Build EVM init code that:
///   - Deploys a runtime contract that:
///     - SSTOREs 0xDEAD into slot 0
///     - emits LOG0 with 32 bytes of data (the value 0xDEAD)
///     - STOPs
///
/// We build the bytecode manually so there are no external compiler dependencies.
fn simple_contract_init_code() -> alloy::primitives::Bytes {
    use revm::state::bytecode::opcode;

    // Runtime bytecode:
    //   PUSH2 0xDEAD   (3 bytes: 61 AD DE)
    //   PUSH1 0x00     (2 bytes: 60 00)
    //   SSTORE         (1 byte:  55)
    //   PUSH2 0xDEAD   (3 bytes: 61 AD DE)
    //   PUSH1 0x00     (2 bytes: 60 00)
    //   MSTORE         (1 byte:  52)
    //   PUSH1 0x20     (2 bytes: 60 20)
    //   PUSH1 0x00     (2 bytes: 60 00)
    //   LOG0           (1 byte:  A0)
    //   STOP           (1 byte:  00)
    // Total runtime = 18 bytes
    let runtime: Vec<u8> = vec![
        opcode::PUSH2, 0xDE, 0xAD,   // value
        opcode::PUSH1, 0x00,          // slot
        opcode::SSTORE,
        opcode::PUSH2, 0xDE, 0xAD,   // value to store in memory
        opcode::PUSH1, 0x00,          // memory offset
        opcode::MSTORE,
        opcode::PUSH1, 0x20,          // log data size
        opcode::PUSH1, 0x00,          // log data offset
        opcode::LOG0,
        opcode::STOP,
    ];
    let runtime_len = runtime.len() as u8;
    let init_code_len = 12u8; // length of the init code below

    // Init code: CODECOPY the runtime then RETURN it
    //   PUSH1 runtime_len
    //   PUSH1 init_code_len     (offset of runtime in the full bytecode)
    //   PUSH1 0x00              (memory dest)
    //   CODECOPY
    //   PUSH1 runtime_len
    //   PUSH1 0x00
    //   RETURN
    let mut init: Vec<u8> = vec![
        opcode::PUSH1, runtime_len,
        opcode::PUSH1, init_code_len,
        opcode::PUSH1, 0x00,
        opcode::CODECOPY,
        opcode::PUSH1, runtime_len,
        opcode::PUSH1, 0x00,
        opcode::RETURN,
    ];
    assert_eq!(init.len(), init_code_len as usize, "init code length mismatch");

    init.extend_from_slice(&runtime);
    alloy::primitives::Bytes::from(init)
}

#[tokio::test]
#[ignore = "requires anvil to be installed and in PATH"]
async fn test_replay_anvil() {
    // ------------------------------------------------------------------ //
    // 1. Start anvil and create provider                                  //
    // ------------------------------------------------------------------ //
    use alloy::node_bindings::Anvil;

    let anvil = Anvil::new().spawn();
    let rpc_url = anvil.endpoint();

    // Set up wallet with first anvil account
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer.clone());
    let deployer_addr = signer.address();

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(rpc_url.parse().unwrap());

    // ------------------------------------------------------------------ //
    // 2. Deploy the simple contract                                       //
    // ------------------------------------------------------------------ //
    let init_bytecode = simple_contract_init_code();

    let deploy_tx = TransactionRequest::default()
        .from(deployer_addr)
        .with_deploy_code(init_bytecode);

    let receipt = provider
        .send_transaction(deploy_tx)
        .await
        .expect("send deploy tx")
        .get_receipt()
        .await
        .expect("deploy receipt");

    assert!(receipt.status(), "contract deployment should succeed");
    let contract_addr = receipt
        .contract_address
        .expect("deployment should return contract address");

    println!("Deployed contract at {:?}", contract_addr);

    // ------------------------------------------------------------------ //
    // 3. Send a transaction that calls the deployed contract              //
    // ------------------------------------------------------------------ //
    let call_tx = TransactionRequest::default()
        .from(deployer_addr)
        .to(contract_addr)
        .with_input(alloy::primitives::Bytes::new()); // empty calldata

    let call_receipt = provider
        .send_transaction(call_tx)
        .await
        .expect("send call tx")
        .get_receipt()
        .await
        .expect("call receipt");

    assert!(call_receipt.status(), "call transaction should succeed");
    let call_tx_hash: TxHash = call_receipt.transaction_hash;

    println!("Call tx hash: {:?}", call_tx_hash);

    // ------------------------------------------------------------------ //
    // 4. Replay via our module                                            //
    // ------------------------------------------------------------------ //
    let exec_data = replay_transaction(&rpc_url, call_tx_hash)
        .await
        .expect("replay should succeed");

    // ------------------------------------------------------------------ //
    // 5. Assertions                                                       //
    // ------------------------------------------------------------------ //
    println!(
        "Replay captured {} steps, {} calls, {} logs",
        exec_data.step_count(),
        exec_data.calls.len(),
        exec_data.logs.len()
    );

    // a. Should have recorded execution steps
    assert!(
        exec_data.step_count() > 0,
        "replay should produce execution steps"
    );

    // b. Should have at least one SSTORE step
    let sstore_step = exec_data
        .steps
        .iter()
        .find(|s| s.opcode_name == "SSTORE");
    assert!(
        sstore_step.is_some(),
        "replay should record SSTORE opcode; got opcodes: {:?}",
        exec_data
            .steps
            .iter()
            .map(|s| s.opcode_name.as_str())
            .collect::<Vec<_>>()
    );

    // c. SSTORE should see 0xDEAD value on stack
    let sstore = sstore_step.unwrap();
    // Stack before SSTORE: top is slot (0), second is value (0xDEAD)
    assert!(
        sstore.stack.len() >= 2,
        "SSTORE should have >= 2 elements on stack"
    );
    // The stack data() is bottom-to-top, so the last element is the top (slot=0),
    // and second-to-last is the value.
    let slot = sstore.stack[sstore.stack.len() - 1];
    let value = sstore.stack[sstore.stack.len() - 2];
    assert_eq!(slot, alloy::primitives::U256::ZERO, "SSTORE slot should be 0");
    assert_eq!(
        value,
        alloy::primitives::U256::from(0xDEADu32),
        "SSTORE value should be 0xDEAD"
    );

    // d. Should have at least one LOG event
    assert!(
        !exec_data.logs.is_empty(),
        "replay should capture LOG0 event"
    );
    let log = &exec_data.logs[0];
    assert_eq!(
        log.address, contract_addr,
        "log should be emitted by the contract"
    );
    assert_eq!(log.topics.len(), 0, "LOG0 has 0 topics");
}
