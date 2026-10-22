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
    /// That block's header timestamp, in seconds.  The derived recording id
    /// takes its time component from here, so it names when the transaction
    /// ran rather than when it was recorded.
    pub block_timestamp: u64,
    /// The transaction this is a replay of.
    pub tx_hash: TxHash,
    /// Its input data: calldata for a call, the initcode for a contract creation.
    pub input: Bytes,
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
    let spec = spec_for_block(chain_id, block_number, &*block.header);

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
        tx_hash,
        input: AlloyTransactionTrait::input(&tx).clone(),
        chain_id,
        block_number,
        block_timestamp: block.header.timestamp(),
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

/// What [`replay_block`] did with a whole block.
#[derive(Debug)]
pub struct BlockReplay {
    pub chain_id: u64,
    pub block_number: u64,
    /// Every transaction the block carries, in order.
    pub tx_hashes: Vec<TxHash>,
    /// How many of them were replayed and handed to the callback.
    pub replayed: usize,
    /// Where the replay stopped early, and why.  Once a transaction cannot
    /// be executed the state every later one would start from is wrong, so
    /// the rest are refused rather than traced from a wrong prestate.
    pub stopped: Option<(usize, String)>,
}

/// Replay EVERY transaction of `block_number`, in order, on one fork pinned
/// at the parent block, handing each one's [`ReplayedTransaction`] to
/// `on_tx` as soon as it has run.
///
/// The same `replay-preceding` prestate as [`replay_transaction_detailed`],
/// paid once per block instead of once per transaction.  Tracing all `n`
/// transactions of a block one at a time replays `n(n-1)/2` of them
/// (31,606 for mainnet blocks 4,370,100-4,370,101, measured with
/// BlockTracer's `capture-eth-range.mjs --plan`); this replays `n`.  Each
/// transaction runs under a fresh `CodeTracerInspector` and its state is
/// committed before the next starts, which is exactly what a preceding
/// replay does, so the trace of transaction `k` is taken from the state the
/// first `k` transactions left.
///
/// The callback runs between transactions and may await (the recording
/// route resolves source over the network).  Results are not collected
/// here because a block's memory-captured steps do not fit comfortably in
/// memory at once.
pub async fn replay_block<F, Fut>(
    rpc_url: &str,
    block_number: u64,
    options: ReplayOptions,
    mut on_tx: F,
) -> Result<BlockReplay>
where
    F: FnMut(ReplayedTransaction) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let provider = build_provider(rpc_url)?;
    let block = provider
        .get_block_by_number(block_number.into())
        .full()
        .await
        .context("eth_getBlockByNumber failed")?
        .ok_or_else(|| eyre::eyre!("block {} not found", block_number))?;
    let chain_id = provider
        .get_chain_id()
        .await
        .context("eth_chainId failed")?;
    if block_number == 0 {
        return Err(eyre::eyre!("block 0 has no parent state to fork from"));
    }
    let parent_block_id: alloy::rpc::types::BlockId = (block_number - 1).into();
    let spec = spec_for_block(chain_id, block_number, &*block.header);
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
    let block_timestamp = block.header.timestamp();
    let transactions = match &block.transactions {
        BlockTransactions::Full(txs) => txs.clone(),
        _ => return Err(eyre::eyre!("block did not return full transactions")),
    };
    let tx_hashes: Vec<TxHash> = transactions.iter().map(|t| t.tx_hash()).collect();

    let meta = BlockchainDbMeta::new(block_env.clone(), rpc_url.to_string());
    let backend = SharedBackend::spawn_backend_thread(
        provider,
        BlockchainDb::new(meta, None),
        Some(parent_block_id),
    );
    type ForkDb = CacheDB<SharedBackend<alloy::network::Ethereum>>;
    let mut ctx: RevmContext<BlockEnv, TxEnv, CfgEnv, ForkDb, Journal<ForkDb>, ()> =
        RevmContext::new(CacheDB::new(backend), spec);
    ctx.modify_block(|b| {
        *b = block_env;
    });
    ctx.modify_cfg(|cfg| {
        cfg.chain_id = chain_id;
    });
    let fresh = || {
        if options.capture_memory {
            CodeTracerInspector::with_memory_capture()
        } else {
            CodeTracerInspector::new()
        }
    };
    let mut evm = ctx.build_mainnet_with_inspector(fresh());

    let mut replayed = 0usize;
    let mut stopped = None;
    for (index, tx) in transactions.iter().enumerate() {
        let tx_env = match alloy_tx_to_revm_tx(tx) {
            Ok(env) => env,
            Err(e) => {
                stopped = Some((index, format!("cannot convert transaction: {e}")));
                break;
            }
        };
        evm.inspector = fresh();
        let outcome = match evm.inspect_tx_commit(tx_env) {
            Ok(outcome) => outcome,
            Err(e) => {
                stopped = Some((index, format!("{e:?}")));
                break;
            }
        };
        let execution =
            std::mem::replace(&mut evm.inspector, CodeTracerInspector::new()).into_execution_data();
        on_tx(ReplayedTransaction {
            execution,
            tx_hash: tx_hashes[index],
            input: AlloyTransactionTrait::input(tx).clone(),
            chain_id,
            block_number,
            block_timestamp,
            tx_index: index,
            spec_id: spec,
            to: AlloyTransactionTrait::to(tx),
            caller: TransactionResponse::from(tx),
            preceding_replayed: index,
            succeeded: outcome.is_success(),
            output: outcome.output().cloned().unwrap_or_default(),
            gas_used: outcome.gas_used(),
        })
        .await?;
        replayed += 1;
    }

    Ok(BlockReplay {
        chain_id,
        block_number,
        tx_hashes,
        replayed,
        stopped,
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
        .layer(
            alloy::transports::layers::RetryBackoffLayer::new_with_policy(
                MAX_RATE_LIMIT_RETRIES,
                INITIAL_BACKOFF_MS,
                0,
                // AND A GATEWAY'S TRANSIENT 5xx. alloy retries 429 and 503 on its own; a free
                // endpoint also answers 408 (drpc's free-plan "Request timeout", which stopped
                // mainnet block 22,500,001 at index 88), 502 / 504 and Cloudflare's 520-524 for a moment and
                // then serves the same read. A whole-block replay makes thousands of reads, so
                // one such answer used to stop a block (mainnet 4,370,100, measured: a 502 at
                // index 59 left 18 transactions not reached) when retrying would have read on.
                alloy::transports::layers::RateLimitRetryPolicy::default().or(
                    |e: &alloy::transports::TransportError| {
                        matches!(
                            e.as_transport_err(),
                            Some(alloy::transports::TransportErrorKind::HttpError(h))
                                if matches!(h.status, 408 | 500 | 502 | 504 | 520..=524)
                        )
                    },
                ),
            ),
        )
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

    // THE TYPED-TRANSACTION FIELDS, which change what the transaction costs. Without them
    // revm ran every transaction as if it carried no access list (EIP-2930), no blobs
    // (EIP-4844) and no authorizations (EIP-7702). An access-listed slot is warm and was
    // charged cold (2,100 / 2,600 gas), and a list entry's own cost went unpaid. In
    // BlockTracer's six pinned yield windows that was 13 transactions whose replayed gas
    // differed from the receipt by exactly such amounts, all of them successful.
    builder = builder.tx_type(Some(alloy::eips::Typed2718::ty(tx)));
    if let Some(list) = AlloyTransactionTrait::access_list(tx) {
        builder = builder.access_list(list.clone());
    }
    if let Some(hashes) = AlloyTransactionTrait::blob_versioned_hashes(tx) {
        builder = builder.blob_hashes(hashes.to_vec());
    }
    if let Some(fee) = AlloyTransactionTrait::max_fee_per_blob_gas(tx) {
        builder = builder.max_fee_per_blob_gas(fee);
    }
    if let Some(auths) = AlloyTransactionTrait::authorization_list(tx) {
        builder = builder.authorization_list_signed(auths.to_vec());
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
/// The hardfork a block executes under.
///
/// FROM THE CHAIN'S OWN SCHEDULE WHERE IT IS KNOWN, because a header cannot say which
/// pre-London fork it belongs to: before EIP-1559 there is no member that differs
/// between Byzantium, Constantinople, Istanbul and Berlin. The header-only rule below
/// called all of them BERLIN, so mainnet block 4,370,100 (Byzantium) replayed with
/// EIP-2929 access costs and the Istanbul repricings: in BlockTracer's first
/// whole-block capture, 58 of 59 traced transactions disagreed with their receipts on
/// gas, three of them reverting where the chain's succeeded. For Ethereum mainnet
/// (chain 1) this reads the activation heights. A zero difficulty marks the Merge on any
/// chain, which the header rule also missed (it said LONDON for Paris blocks). Other
/// chains keep the header rule and its stated pre-London limitation.
fn spec_for_block<H: BlockHeader>(chain_id: u64, number: u64, header: &H) -> SpecId {
    let by_header = spec_from_block_header(header);
    if by_header >= SpecId::SHANGHAI {
        return by_header;
    }
    if header.difficulty().is_zero() && header.base_fee_per_gas().is_some() {
        return SpecId::MERGE;
    }
    if chain_id != 1 {
        return by_header;
    }
    // Ethereum mainnet activation heights (EIP-7568's hardfork meta list).
    const MAINNET: &[(u64, SpecId)] = &[
        (15_050_000, SpecId::GRAY_GLACIER),
        (13_773_000, SpecId::ARROW_GLACIER),
        (12_965_000, SpecId::LONDON),
        (12_244_000, SpecId::BERLIN),
        (9_200_000, SpecId::MUIR_GLACIER),
        (9_069_000, SpecId::ISTANBUL),
        (7_280_000, SpecId::PETERSBURG),
        (4_370_000, SpecId::BYZANTIUM),
        (2_675_000, SpecId::SPURIOUS_DRAGON),
        (2_463_000, SpecId::TANGERINE),
        (1_920_000, SpecId::DAO_FORK),
        (1_150_000, SpecId::HOMESTEAD),
    ];
    MAINNET
        .iter()
        .find(|(height, _)| number >= *height)
        .map(|(_, spec)| *spec)
        .unwrap_or(SpecId::FRONTIER)
}

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
    fn mainnet_forks_come_from_the_schedule_not_from_the_header() {
        // A pre-London header: no base fee, no withdrawals, a non-zero difficulty. The
        // header alone says BERLIN whatever its height, which is the defect.
        let pre_london = alloy::consensus::Header {
            difficulty: U256::from(3_000_000_000_000u64),
            ..Default::default()
        };
        assert_eq!(spec_from_block_header(&pre_london), SpecId::BERLIN);
        assert_eq!(spec_for_block(1, 4_370_100, &pre_london), SpecId::BYZANTIUM);
        assert_eq!(
            spec_for_block(1, 4_369_999, &pre_london),
            SpecId::SPURIOUS_DRAGON
        );
        assert_eq!(spec_for_block(1, 9_069_000, &pre_london), SpecId::ISTANBUL);
        assert_eq!(spec_for_block(1, 12_500_000, &pre_london), SpecId::BERLIN);
        // CONTROL: another chain keeps the header rule, since its schedule is not known here.
        assert_eq!(spec_for_block(10, 4_370_100, &pre_london), SpecId::BERLIN);
        // The Merge, from the zero difficulty a post-Merge header carries.
        let paris = alloy::consensus::Header {
            base_fee_per_gas: Some(10),
            ..Default::default()
        };
        assert_eq!(spec_for_block(1, 16_000_000, &paris), SpecId::MERGE);
        // And a header that names its fork outright is believed.
        let cancun = alloy::consensus::Header {
            base_fee_per_gas: Some(10),
            withdrawals_root: Some(Default::default()),
            excess_blob_gas: Some(0),
            ..Default::default()
        };
        assert_eq!(spec_for_block(1, 19_500_000, &cancun), SpecId::CANCUN);
    }

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
