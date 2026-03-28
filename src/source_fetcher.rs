//! Stub for fetching verified contract source from Sourcify / Etherscan.
//!
//! The full implementation will query the Sourcify API
//! (<https://docs.sourcify.dev/docs/api/server/>) to download compilation
//! artifacts for a contract at a known address on a given chain.

use alloy::primitives::Address;

use crate::contract_registry::ContractArtifacts;

/// Fetch verified contract source and compilation artifacts for an address.
///
/// Currently a stub — always returns `Ok(None)`.  A real implementation
/// would call:
///
/// ```text
/// GET https://sourcify.dev/server/v2/contract/{chain_id}/{address}
/// ```
///
/// and deserialize the returned JSON into a [`ContractArtifacts`] value.
pub async fn fetch_contract_source(
    _address: Address,
    _chain_id: u64,
) -> eyre::Result<Option<ContractArtifacts>> {
    // TODO: Implement Sourcify API integration
    // https://docs.sourcify.dev/docs/api/server/
    // GET /v2/contract/{chain}/{address}
    Ok(None)
}
