//! RPC client for fetching `debug_traceTransaction` struct logs from an EVM node.

use alloy::primitives::TxHash;
use alloy::providers::ProviderBuilder;
use alloy::providers::ext::DebugApi;
use alloy::rpc::types::trace::geth::{
    DefaultFrame, GethDebugTracingOptions, GethDefaultTracingOptions, GethTrace,
};

use crate::structlog::StructLog;

/// Fetch the structLog entries for a given transaction hash from an EVM node.
///
/// Connects to `rpc_url`, calls `debug_traceTransaction` with the default
/// (structLog) tracer, and returns the resulting log entries for processing.
pub async fn fetch_struct_logs(rpc_url: &str, tx_hash: TxHash) -> eyre::Result<DefaultFrame> {
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let opts = GethDebugTracingOptions {
        config: GethDefaultTracingOptions {
            disable_storage: Some(false),
            disable_stack: Some(false),
            enable_memory: Some(true),
            enable_return_data: Some(true),
            ..Default::default()
        },
        ..Default::default()
    };

    let trace = provider
        .debug_trace_transaction(tx_hash, opts)
        .await
        .map_err(|e| eyre::eyre!("debug_traceTransaction failed: {}", e))?;

    match trace {
        GethTrace::Default(frame) => Ok(frame),
        _other => Err(eyre::eyre!(
            "expected Default (structLog) trace, got a different tracer response"
        )),
    }
}

/// Extract the struct log entries from a [`DefaultFrame`].
pub fn extract_struct_logs(frame: &DefaultFrame) -> &[StructLog] {
    &frame.struct_logs
}
