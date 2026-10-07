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
use alloy::primitives::{Address, Bytes, TxHash, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::BlockTransactions;
use eyre::{Context, Result};
use foundry_fork_db::{BlockchainDb, SharedBackend, cache::BlockchainDbMeta};
use revm::{
    Context as RevmContext,
    context::{BlockEnv, CfgEnv, Journal, TxEnv},
    database::CacheDB,
    handler::{ExecuteCommitEvm, MainBuilder},
    inspector::{InspectCommitEvm, NoOpInspector},
    primitives::{TxKind, hardfork::SpecId},
};

use crate::inspector::{CodeTracerInspector, ExecutionData};

/// Knobs for [`replay_transaction_detailed`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ReplayOptions {
    /// Copy the executing frame's memory into every captured step.
    ///
    /// Needed by the on-chain recording route (`onchain.rs`), which rebuilds
    /// structLogs from the inspector's output; the recorder reads memory to
    /// decode `string` / `bytes` locals and `LOG*` payloads.  Off by default
    /// because it is the dominant cost of a replay.
    pub capture_memory: bool,
}

/// A replayed transaction: the inspector's output plus the chain context the
/// recording route needs and cannot re-derive without a second round trip.
#[derive(Debug)]
pub struct ReplayedTransaction {
    /// Steps / calls / logs captured from the target transaction.
    pub execution: ExecutionData,
    /// `eth_chainId` of the endpoint the replay ran against.
    pub chain_id: u64,
    /// Block the target transaction was mined in.
    pub block_number: u64,
    /// Index of the target transaction within that block.
    pub tx_index: usize,
    /// The revm hardfork the replay ran under, as inferred from the block
    /// header by [`spec_from_block_header`].
    pub spec_id: SpecId,
    /// `to` of the target transaction; `None` for a contract creation.
    pub to: Option<Address>,
    /// `from` of the target transaction.
    pub caller: Address,
    /// How many preceding transactions in the block were replayed to build
    /// the prestate.  Equals `tx_index` on success — the declared
    /// `replay-preceding` prestate strategy.
    pub preceding_replayed: usize,
    /// Whether the target transaction's replay succeeded (EIP-658 status 1).
    pub succeeded: bool,
    /// Return data / revert payload of the target transaction.
    pub output: Bytes,
    /// Gas the replay charged the target transaction.
    pub gas_used: u64,
}

/// Replay a transaction identified by `tx_hash` against a forked chain state
/// fetched from `rpc_url`.
///
/// Returns the collected [`ExecutionData`] from the target transaction.
/// Thin wrapper over [`replay_transaction_detailed`] with default options,
/// kept because it is the published entry point.
pub async fn replay_transaction(rpc_url: &str, tx_hash: TxHash) -> Result<ExecutionData> {
    Ok(
        replay_transaction_detailed(rpc_url, tx_hash, ReplayOptions::default())
            .await?
            .execution,
    )
}

/// Replay `tx_hash` and return the inspector's output together with the
/// chain context (chain id, block, index, hardfork, participants, outcome).
///
/// Same forked-state mechanics as [`replay_transaction`]; see the module
/// docs for the four steps.
pub async fn replay_transaction_detailed(
    rpc_url: &str,
    tx_hash: TxHash,
    options: ReplayOptions,
) -> Result<ReplayedTransaction> {
    // ------------------------------------------------------------------ //
    // 1.  Fetch the transaction and its block via alloy                   //
    // ------------------------------------------------------------------ //
    let provider = build_provider(rpc_url)?;

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

    // The hardfork has to be known BEFORE the block environment is built:
    // the blob base fee is priced with a per-fork update fraction, and
    // `BlobExcessGasAndPrice::new` computes that price eagerly.
    let spec = spec_from_block_header(&*block.header);

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
                blob_base_fee_update_fraction(spec),
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

    let mut preceding_replayed = 0usize;
    for (i, prior_tx) in transactions.iter().enumerate() {
        if i >= tx_index {
            break;
        }
        let tx_env = alloy_tx_to_revm_tx(prior_tx)?;
        evm.transact_commit(tx_env).map_err(|e| {
            eyre::eyre!(
                "failed to replay prior tx {} of {} in block {} ({}): {:?} — \
                 the prestate for the target transaction would be wrong, so this \
                 is refused rather than traced",
                i,
                tx_index,
                block_number,
                prior_tx.tx_hash(),
                e
            )
        })?;
        preceding_replayed += 1;
    }

    // ------------------------------------------------------------------ //
    // 5.  Replay the target transaction with CodeTracerInspector          //
    // ------------------------------------------------------------------ //
    let target_tx = &transactions[tx_index];
    let target_tx_env = alloy_tx_to_revm_tx(target_tx)?;

    // Switch to a CodeTracerInspector for the target transaction.
    // `with_inspector` changes the EVM's inspector type; then `inspect_tx_commit`
    // executes with the already-set inspector and commits the resulting state.
    let inspector = if options.capture_memory {
        CodeTracerInspector::with_memory_capture()
    } else {
        CodeTracerInspector::new()
    };
    let mut tracing_evm = evm.with_inspector(inspector);
    let outcome = tracing_evm
        .inspect_tx_commit(target_tx_env)
        .map_err(|e| eyre::eyre!("failed to replay target tx: {:?}", e))?;

    let succeeded = outcome.is_success();
    let gas_used = outcome.gas_used();
    let output = outcome.output().cloned().unwrap_or_default();

    let data = tracing_evm.into_inspector().into_execution_data();
    Ok(ReplayedTransaction {
        execution: data,
        chain_id,
        block_number,
        tx_index,
        spec_id: spec,
        to: AlloyTransactionTrait::to(&tx),
        caller: TransactionResponse::from(&tx),
        preceding_replayed,
        succeeded,
        output,
        gas_used,
    })
}

/// Build the HTTP provider the replay reads state through, with
/// rate-limit retry in front of it.
///
/// Why the layer is not optional: reconstructing the prestate of a mainnet
/// transaction at index N replays N transactions, and each one faults in
/// accounts and storage slots one JSON-RPC call at a time.  The USDT
/// transfer this route was first proven on (block 26,083,328, index 8)
/// issued enough `eth_getBalance` / `eth_getCode` / `eth_getStorageAt`
/// reads to earn `HTTP 429 ... Public endpoint rate limit` from an
/// endpoint that answers every one of those methods happily in isolation.
/// Without retry the whole replay aborts on the first throttled read, and
/// the failure reads like an endpoint that cannot serve archive state when
/// in fact it can.
///
/// `compute_units_per_second = 0` disables alloy's own client-side CU
/// throttle: the public endpoints this runs against publish no CU budget,
/// so a guessed one would only slow the common case.  Retries are driven by
/// the endpoint's actual 429s.
pub fn build_provider(rpc_url: &str) -> Result<impl Provider + Clone + use<>> {
    /// Retries per throttled request.  Chosen so that a replay survives a
    /// sustained throttle rather than only a momentary one.
    const MAX_RATE_LIMIT_RETRIES: u32 = 12;
    /// Initial backoff in milliseconds; alloy escalates from here.
    const INITIAL_BACKOFF_MS: u64 = 500;

    let client = alloy::rpc::client::ClientBuilder::default()
        .layer(alloy::transports::layers::RetryBackoffLayer::new(
            MAX_RATE_LIMIT_RETRIES,
            INITIAL_BACKOFF_MS,
            0,
        ))
        .http(rpc_url.parse()?);
    Ok(ProviderBuilder::new().connect_client(client))
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

/// The EIP-4844 blob-base-fee update fraction in force at `spec`.
///
/// EIP-7691 (Prague) raised the fraction from 3,338,477 to 5,007,716 along
/// with the blob target.  Pricing a Prague block with the Cancun fraction is
/// not a rounding difference: `fake_exponential` is exponential in
/// `excess_blob_gas / fraction`, so the smaller denominator inflates the
/// result by orders of magnitude.
///
/// This was a live defect rather than a theoretical one.  The Cancun
/// fraction was hard-coded here, and mainnet's post-Prague excess blob gas
/// (220,201,152 at block 26,083,328, measured) divided by the Cancun
/// fraction puts `fake_exponential`'s accumulator past `u128::MAX` — the
/// replay aborted with "attempt to multiply with overflow" inside revm
/// before it reached a single opcode.  Nothing caught it because the only
/// replay test ran against anvil, whose fresh chain reports
/// `excess_blob_gas = 0`.
fn blob_base_fee_update_fraction(spec: SpecId) -> u64 {
    if spec >= SpecId::PRAGUE {
        revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE
    } else {
        revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context_interface::block::BlobExcessGasAndPrice;
    use revm::primitives::eip4844::{
        BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN, BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE,
    };

    #[test]
    fn prague_and_later_price_blobs_with_the_eip_7691_fraction() {
        assert_eq!(
            blob_base_fee_update_fraction(SpecId::PRAGUE),
            BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE
        );
        assert_eq!(
            blob_base_fee_update_fraction(SpecId::CANCUN),
            BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN
        );
        // Pre-blob forks never read the value, but the fallback must be the
        // first fraction that ever existed rather than the newest.
        assert_eq!(
            blob_base_fee_update_fraction(SpecId::SHANGHAI),
            BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN
        );
    }

    /// Regression: a real mainnet post-Prague `excessBlobGas` must price
    /// without overflowing.
    ///
    /// 220,201,152 is the value in the header of mainnet block 26,083,328
    /// (`0xd2000c0`), the block whose replay first hit this.  With the
    /// Cancun fraction the same call panics inside revm's
    /// `fake_exponential`, which is why the fraction is selected by fork
    /// rather than fixed.
    #[test]
    fn a_real_mainnet_post_prague_excess_blob_gas_prices_without_overflow() {
        let excess_blob_gas: u64 = 0xd2000c0;
        assert_eq!(excess_blob_gas, 220_201_152);
        let priced = BlobExcessGasAndPrice::new(
            excess_blob_gas,
            blob_base_fee_update_fraction(SpecId::PRAGUE),
        );
        assert_eq!(priced.excess_blob_gas, excess_blob_gas);
        assert!(
            priced.blob_gasprice > 0,
            "a non-zero excess blob gas must price above the 1 wei minimum"
        );
    }
}
