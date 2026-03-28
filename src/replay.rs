//! On-chain transaction replay using revm + foundry-fork-db.
//!
//! Given a transaction hash and an RPC URL this module:
//! 1. Fetches the transaction and its containing block from the RPC.
//! 2. Creates a forked EVM state pinned at the *parent* block (state
//!    just before the target block was mined).
//! 3. Replays every preceding transaction in the block with a no-op inspector
//!    so the pre-state for the target transaction is correct.
//! 4. Replays the target transaction with a `CodeTracerInspector`, collecting
//!    detailed step, call, and log data.

use alloy::consensus::BlockHeader;
use alloy::consensus::Transaction as AlloyTransactionTrait;
use alloy::network::TransactionResponse;
use alloy::primitives::{TxHash, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::BlockTransactions;
use eyre::{Context, Result};
use foundry_fork_db::{cache::BlockchainDbMeta, BlockchainDb, SharedBackend};
use revm::{
    context::{BlockEnv, CfgEnv, Journal, TxEnv},
    database::CacheDB,
    handler::{ExecuteCommitEvm, MainBuilder},
    inspector::{InspectCommitEvm, NoOpInspector},
    primitives::{hardfork::SpecId, TxKind},
    Context as RevmContext,
};

use crate::inspector::{CodeTracerInspector, ExecutionData};

/// Replay a transaction identified by `tx_hash` against a forked chain state
/// fetched from `rpc_url`.
///
/// Returns the collected [`ExecutionData`] from the target transaction.
pub async fn replay_transaction(rpc_url: &str, tx_hash: TxHash) -> Result<ExecutionData> {
    // ------------------------------------------------------------------ //
    // 1.  Fetch the transaction and its block via alloy                   //
    // ------------------------------------------------------------------ //
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let tx = provider
        .get_transaction_by_hash(tx_hash)
        .await
        .context("eth_getTransactionByHash failed")?
        .ok_or_else(|| eyre::eyre!("transaction {} not found", tx_hash))?;

    let block_number = tx
        .block_number()
        .ok_or_else(|| eyre::eyre!("transaction is pending (no block number)"))?;

    let tx_index = tx
        .transaction_index()
        .ok_or_else(|| eyre::eyre!("transaction has no index"))? as usize;

    // Fetch the full block (with transactions) so we can replay predecessors.
    let block = provider
        .get_block_by_number(block_number.into())
        .full()
        .await
        .context("eth_getBlockByNumber failed")?
        .ok_or_else(|| eyre::eyre!("block {} not found", block_number))?;

    // Fetch the chain ID early (before provider is moved into SharedBackend).
    let chain_id = provider
        .get_chain_id()
        .await
        .context("eth_chainId failed")?;

    // ------------------------------------------------------------------ //
    // 2.  Build a forked state pinned at the *parent* block               //
    // ------------------------------------------------------------------ //
    // Pin to parent block so the state reflects what it was at the start
    // of the target block.
    let parent_block_id: alloy::rpc::types::BlockId = (block_number - 1).into();

    // Build block environment from the target block header.
    let block_env = BlockEnv {
        number: U256::from(block_number),
        beneficiary: block.header.beneficiary(),
        timestamp: U256::from(block.header.timestamp()),
        difficulty: block.header.difficulty(),
        basefee: block.header.base_fee_per_gas().unwrap_or(0),
        gas_limit: block.header.gas_limit(),
        prevrandao: block.header.mix_hash(),
        blob_excess_gas_and_price: block.header.excess_blob_gas().map(|ebg| {
            revm::context_interface::block::BlobExcessGasAndPrice::new(
                ebg,
                revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
            )
        }),
    };

    let meta = BlockchainDbMeta::new(block_env.clone(), rpc_url.to_string());
    let blockchain_db = BlockchainDb::new(meta, None);

    // spawn_backend_thread launches a background thread with its own tokio
    // runtime to service database requests.
    let backend =
        SharedBackend::spawn_backend_thread(provider, blockchain_db, Some(parent_block_id));
    // Wrap in CacheDB so we get mutable (DatabaseCommit) semantics on top of
    // the read-only SharedBackend.
    let db = CacheDB::new(backend);

    // ------------------------------------------------------------------ //
    // 3.  Build the revm EVM                                              //
    // ------------------------------------------------------------------ //
    let spec = spec_from_block_header(&*block.header);

    type ForkDb = CacheDB<SharedBackend<alloy::network::Ethereum>>;
    let mut ctx: RevmContext<BlockEnv, TxEnv, CfgEnv, ForkDb, Journal<ForkDb>, ()> =
        RevmContext::new(db, spec);
    ctx.modify_block(|b| {
        *b = block_env;
    });
    ctx.modify_cfg(|cfg| {
        cfg.chain_id = chain_id;
    });

    let mut evm = ctx.build_mainnet_with_inspector(NoOpInspector);

    // ------------------------------------------------------------------ //
    // 4.  Replay preceding transactions (no-op inspector)                 //
    // ------------------------------------------------------------------ //
    let transactions = match &block.transactions {
        BlockTransactions::Full(txs) => txs,
        _ => return Err(eyre::eyre!("block did not return full transactions")),
    };

    for (i, prior_tx) in transactions.iter().enumerate() {
        if i >= tx_index {
            break;
        }
        let tx_env = alloy_tx_to_revm_tx(prior_tx)?;
        evm.transact_commit(tx_env)
            .map_err(|e| eyre::eyre!("failed to replay prior tx {}: {:?}", i, e))?;
    }

    // ------------------------------------------------------------------ //
    // 5.  Replay the target transaction with CodeTracerInspector          //
    // ------------------------------------------------------------------ //
    let target_tx = &transactions[tx_index];
    let target_tx_env = alloy_tx_to_revm_tx(target_tx)?;

    // Switch to a CodeTracerInspector for the target transaction.
    // `with_inspector` changes the EVM's inspector type; then `inspect_tx_commit`
    // executes with the already-set inspector and commits the resulting state.
    let mut tracing_evm = evm.with_inspector(CodeTracerInspector::new());
    tracing_evm
        .inspect_tx_commit(target_tx_env)
        .map_err(|e| eyre::eyre!("failed to replay target tx: {:?}", e))?;

    let data = tracing_evm.into_inspector().into_execution_data();
    Ok(data)
}

/// Convert an alloy `TransactionResponse` into a revm `TxEnv`.
fn alloy_tx_to_revm_tx<T>(tx: &T) -> Result<TxEnv>
where
    T: AlloyTransactionTrait + TransactionResponse,
{
    let kind = match AlloyTransactionTrait::to(tx) {
        Some(addr) => TxKind::Call(addr),
        None => TxKind::Create,
    };

    // For legacy transactions gas_price() is Some; for EIP-1559 use max_fee_per_gas().
    let gas_price = AlloyTransactionTrait::gas_price(tx)
        .unwrap_or_else(|| AlloyTransactionTrait::max_fee_per_gas(tx));

    let mut builder = TxEnv::builder()
        .caller(TransactionResponse::from(tx))
        .gas_limit(AlloyTransactionTrait::gas_limit(tx))
        .gas_price(gas_price)
        .kind(kind)
        .value(AlloyTransactionTrait::value(tx))
        .data(AlloyTransactionTrait::input(tx).clone())
        .nonce(AlloyTransactionTrait::nonce(tx));

    if let Some(fee) = AlloyTransactionTrait::max_priority_fee_per_gas(tx) {
        builder = builder.gas_priority_fee(Some(fee));
    }

    if let Some(chain_id) = AlloyTransactionTrait::chain_id(tx) {
        builder = builder.chain_id(Some(chain_id));
    }

    Ok(builder.build_fill())
}

/// Infer the appropriate revm SpecId from a block header by inspecting which
/// optional fields are present.  The heuristic checks fields introduced at
/// each hard fork in order from newest to oldest:
///
/// | Field              | Introduced at |
/// |--------------------|---------------|
/// | `requests_hash`    | Prague        |
/// | `excess_blob_gas`  | Cancun        |
/// | `withdrawals_root` | Shanghai      |
/// | `base_fee_per_gas` | London        |
///
/// Blocks before London have none of the above fields and are treated as
/// BERLIN (a safe conservative default for pre-EIP-1559 chains).
///
/// Note: MERGE/Paris is not detectable from the header alone (it shares the
/// same optional fields as London).  The distinction only matters for
/// prevrandao semantics, which are handled by the block environment rather
/// than the spec ID.
fn spec_from_block_header<H: BlockHeader>(header: &H) -> SpecId {
    if header.requests_hash().is_some() {
        SpecId::PRAGUE
    } else if header.excess_blob_gas().is_some() {
        SpecId::CANCUN
    } else if header.withdrawals_root().is_some() {
        SpecId::SHANGHAI
    } else if header.base_fee_per_gas().is_some() {
        SpecId::LONDON
    } else {
        SpecId::BERLIN
    }
}
