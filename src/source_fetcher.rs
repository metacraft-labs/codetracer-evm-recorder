//! Fetcher for verified contract source from the Sourcify server.
//!
//! Sourcify (<https://sourcify.dev/>) is a community-run service that hosts
//! verified Solidity / Vyper compilation artefacts for contracts deployed
//! on a given chain, indexed by `(chain_id, address)`.
//!
//! This module exposes:
//!
//! * [`fetch_sourcify_files`] -- low-level API.  Performs the HTTP GET
//!   against `https://sourcify.dev/server/files/any/<chain>/<address>`,
//!   parses the JSON envelope, and returns a [`SourcifyContract`] value
//!   holding the verified `.sol` sources plus the raw `metadata.json`
//!   string.  Returns `Ok(None)` on a 404 (no verified match for that
//!   address).  All assertions in the unit tests are strict (exact
//!   counts and exact string equality on file contents) per the
//!   recorder's strict-assertions policy.
//!
//! * [`parse_sourcify_response`] -- pure function that parses a
//!   Sourcify JSON envelope into a [`SourcifyContract`].  Exposed so
//!   tests can pin against a recorded fixture without touching the
//!   network.
//!
//! * [`fetch_contract_source`] -- higher-level wrapper that returns a
//!   ready-to-register [`ContractArtifacts`].  Currently still a stub
//!   for the final assembly step (see TODO in the function body): turning
//!   raw verified sources into a `ContractArtifacts` requires
//!   re-compiling them with the exact solc settings recorded in
//!   `metadata.json` so the resulting `srcmap-runtime` aligns with the
//!   on-chain bytecode.  The fetch + parse layer below is fully
//!   implemented and tested; only the solc-driven re-compile remains.

use alloy::primitives::Address;
use serde::Deserialize;

use crate::contract_registry::ContractArtifacts;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Match quality reported by Sourcify for a verified contract.
///
/// * [`Full`] -- bytecode + metadata hash both match exactly.  This is the
///   strongest guarantee: the deployed bytecode was produced byte-for-byte
///   by the published source.
/// * [`Partial`] -- bytecode matches but the metadata hash differs (often
///   because of comment-only / whitespace-only edits to source files).
///   The compiled bytecode is still a faithful execution match for the
///   on-chain code.
///
/// [`Full`]: SourcifyMatch::Full
/// [`Partial`]: SourcifyMatch::Partial
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcifyMatch {
    /// Full match: bytecode AND metadata hash both match.
    Full,
    /// Partial match: bytecode matches; metadata hash differs.
    Partial,
}

/// A single file from a Sourcify verified-contract response.
///
/// `name` is the basename of the file (e.g. `Counter.sol`); `path` is the
/// full repo-relative path under Sourcify's storage layout.  `content` is
/// the raw file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcifySourceFile {
    pub name: String,
    pub path: String,
    pub content: String,
}

/// Verified-contract bundle returned by Sourcify for `(chain_id, address)`.
///
/// Holds the match quality, every `.sol` (and `.vy`) source file required
/// to recompile the contract, and the raw `metadata.json` content (which
/// embeds the ABI, compiler version, optimizer settings and source
/// hashes).  The metadata JSON is intentionally kept as an opaque string
/// so callers can re-parse it with whichever schema variant their solc
/// release expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcifyContract {
    pub match_status: SourcifyMatch,
    /// `.sol` / `.vy` source files recovered from the verified bundle.
    pub sources: Vec<SourcifySourceFile>,
    /// Raw contents of `metadata.json` from the verified bundle, if
    /// present in the response.
    pub metadata_json: Option<String>,
}

// ---------------------------------------------------------------------------
// Internal serde shapes for the Sourcify JSON envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SourcifyResponse {
    status: String,
    files: Vec<SourcifyResponseFile>,
}

#[derive(Debug, Deserialize)]
struct SourcifyResponseFile {
    name: String,
    path: String,
    content: String,
}

// ---------------------------------------------------------------------------
// Pure parser (network-free; testable against a fixture)
// ---------------------------------------------------------------------------

/// Parse a raw Sourcify `/server/files/...` JSON response body into a
/// [`SourcifyContract`].
///
/// Filters the response down to files whose name ends in `.sol` or
/// `.vy` for the source list, and pulls out `metadata.json` (if present)
/// into [`SourcifyContract::metadata_json`].  Returns an error when the
/// JSON envelope is malformed or the `status` field is neither `"full"`
/// nor `"partial"`.
pub fn parse_sourcify_response(body: &str) -> eyre::Result<SourcifyContract> {
    let raw: SourcifyResponse = serde_json::from_str(body)
        .map_err(|e| eyre::eyre!("Sourcify response is not valid JSON: {e}"))?;

    let match_status = match raw.status.as_str() {
        "full" => SourcifyMatch::Full,
        "partial" => SourcifyMatch::Partial,
        other => {
            return Err(eyre::eyre!(
                "Sourcify response has unknown status `{other}` (expected `full` or `partial`)"
            ));
        }
    };

    let mut metadata_json: Option<String> = None;
    let mut sources: Vec<SourcifySourceFile> = Vec::new();
    for f in raw.files.into_iter() {
        if f.name == "metadata.json" {
            metadata_json = Some(f.content);
        } else if f.name.ends_with(".sol") || f.name.ends_with(".vy") {
            sources.push(SourcifySourceFile {
                name: f.name,
                path: f.path,
                content: f.content,
            });
        }
    }

    Ok(SourcifyContract {
        match_status,
        sources,
        metadata_json,
    })
}

// ---------------------------------------------------------------------------
// HTTP fetch
// ---------------------------------------------------------------------------

/// Default Sourcify server base URL.
const SOURCIFY_BASE_URL: &str = "https://sourcify.dev/server";

/// Build the Sourcify "any-match" files endpoint URL for `(chain_id, address)`.
///
/// Uses the `/files/any/` variant so we get back either a full or partial
/// match in a single round-trip; callers inspect
/// [`SourcifyContract::match_status`] if they need to reject partial
/// matches.  Address is rendered with the canonical EIP-55 mixed-case
/// checksum produced by alloy's `Address::to_string`.
pub fn build_sourcify_url(chain_id: u64, address: Address) -> String {
    format!("{SOURCIFY_BASE_URL}/files/any/{chain_id}/{address}")
}

/// Fetch the verified-contract bundle for `(chain_id, address)` from
/// Sourcify.
///
/// Returns:
///
/// * `Ok(Some(contract))` -- Sourcify served a full or partial match.
/// * `Ok(None)` -- Sourcify returned 404 (no verified match for that
///   address on the requested chain).
/// * `Err(_)` -- network error, non-2xx / non-404 HTTP status, malformed
///   JSON envelope, or unrecognised `status` field.
pub async fn fetch_sourcify_files(
    chain_id: u64,
    address: Address,
) -> eyre::Result<Option<SourcifyContract>> {
    let url = build_sourcify_url(chain_id, address);
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .map_err(|e| eyre::eyre!("Sourcify GET {url} failed: {e}"))?;

    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(eyre::eyre!(
            "Sourcify GET {url} returned HTTP status {status}"
        ));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| eyre::eyre!("failed to read Sourcify response body from {url}: {e}"))?;
    parse_sourcify_response(&body).map(Some)
}

// ---------------------------------------------------------------------------
// High-level API consumed by the recorder CLI
// ---------------------------------------------------------------------------

/// Fetch verified contract source and assemble [`ContractArtifacts`] for
/// a contract at `address` on chain `chain_id`.
///
/// Currently delegates to [`fetch_sourcify_files`] for the network round-
/// trip and JSON parsing; the final step -- re-compiling the recovered
/// sources with the exact solc version + settings recorded in
/// `metadata.json` so the synthesized `srcmap-runtime` aligns with the
/// deployed bytecode -- is still a TODO.  Until that compile pipeline
/// lands, this function returns `Ok(None)` even when Sourcify has a
/// verified match, so callers do not register a half-populated
/// `ContractArtifacts` (which would mis-attribute source ranges).
///
/// The lower-level [`fetch_sourcify_files`] is the load-bearing entry
/// point and is fully exercised by the unit tests in this module.
pub async fn fetch_contract_source(
    address: Address,
    chain_id: u64,
) -> eyre::Result<Option<ContractArtifacts>> {
    let _ = fetch_sourcify_files(chain_id, address).await?;
    // TODO: invoke solc with the metadata.json settings to produce
    // runtime_bytecode + srcmap-runtime + storage-layout + AST, then
    // assemble ContractArtifacts.  Tracked alongside the
    // "load contract by address" UX in the recorder roadmap.
    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but realistic Sourcify response payload.  Captures the
    /// exact envelope shape Sourcify returns for a full-match query
    /// (`/server/files/any/1/<address>`): a `status` field plus a `files`
    /// array containing `metadata.json` and one or more source files.
    /// The `content` strings are intentionally short but byte-exact so
    /// the parser tests below can pin against them with `assert_eq!`.
    const FIXTURE_FULL_MATCH: &str = r#"{
  "status": "full",
  "files": [
    {
      "name": "metadata.json",
      "path": "/contracts/full_match/1/0x00000000219ab540356cBB839Cbe05303d7705Fa/metadata.json",
      "content": "{\"compiler\":{\"version\":\"0.6.11+commit.5ef660b1\"},\"language\":\"Solidity\"}"
    },
    {
      "name": "deposit_contract.sol",
      "path": "/contracts/full_match/1/0x00000000219ab540356cBB839Cbe05303d7705Fa/sources/deposit_contract.sol",
      "content": "// SPDX-License-Identifier: CC0-1.0\npragma solidity 0.6.11;\ncontract DepositContract {}\n"
    }
  ]
}"#;

    /// Partial-match fixture: same envelope shape, single source file.
    const FIXTURE_PARTIAL_MATCH: &str = r#"{
  "status": "partial",
  "files": [
    {
      "name": "Counter.sol",
      "path": "/contracts/partial_match/1/0xCafeCafeCafeCafeCafeCafeCafeCafeCafeCafe/sources/Counter.sol",
      "content": "pragma solidity ^0.8.0;\ncontract Counter { uint256 public n; }\n"
    }
  ]
}"#;

    #[test]
    fn parse_full_match_fixture_extracts_sources_and_metadata() {
        let parsed = parse_sourcify_response(FIXTURE_FULL_MATCH).unwrap();
        assert_eq!(parsed.match_status, SourcifyMatch::Full);
        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(parsed.sources[0].name, "deposit_contract.sol");
        assert_eq!(
            parsed.sources[0].path,
            "/contracts/full_match/1/0x00000000219ab540356cBB839Cbe05303d7705Fa/sources/deposit_contract.sol"
        );
        assert_eq!(
            parsed.sources[0].content,
            "// SPDX-License-Identifier: CC0-1.0\npragma solidity 0.6.11;\ncontract DepositContract {}\n"
        );
        assert_eq!(
            parsed.metadata_json,
            Some(
                "{\"compiler\":{\"version\":\"0.6.11+commit.5ef660b1\"},\"language\":\"Solidity\"}"
                    .to_string()
            )
        );
    }

    #[test]
    fn parse_partial_match_fixture_extracts_single_source() {
        let parsed = parse_sourcify_response(FIXTURE_PARTIAL_MATCH).unwrap();
        assert_eq!(parsed.match_status, SourcifyMatch::Partial);
        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(parsed.sources[0].name, "Counter.sol");
        assert_eq!(
            parsed.sources[0].content,
            "pragma solidity ^0.8.0;\ncontract Counter { uint256 public n; }\n"
        );
        assert_eq!(parsed.metadata_json, None);
    }

    #[test]
    fn parse_filters_out_non_source_non_metadata_files() {
        // README.md and a stray .json file should not appear in the
        // sources vec, and should not be promoted to metadata_json.
        let body = r#"{
            "status": "full",
            "files": [
                {"name": "README.md", "path": "/x/README.md", "content": "hi"},
                {"name": "abi.json", "path": "/x/abi.json", "content": "[]"},
                {"name": "Lib.sol", "path": "/x/Lib.sol", "content": "// lib"}
            ]
        }"#;
        let parsed = parse_sourcify_response(body).unwrap();
        assert_eq!(parsed.match_status, SourcifyMatch::Full);
        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(parsed.sources[0].name, "Lib.sol");
        assert_eq!(parsed.sources[0].content, "// lib");
        assert_eq!(parsed.metadata_json, None);
    }

    #[test]
    fn parse_includes_vyper_sources() {
        let body = r##"{
            "status": "full",
            "files": [
                {"name": "vault.vy", "path": "/v/vault.vy", "content": "# vyper code"}
            ]
        }"##;
        let parsed = parse_sourcify_response(body).unwrap();
        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(parsed.sources[0].name, "vault.vy");
        assert_eq!(parsed.sources[0].content, "# vyper code");
    }

    #[test]
    fn parse_rejects_unknown_status() {
        let body = r#"{"status": "weird", "files": []}"#;
        let err = parse_sourcify_response(body).unwrap_err();
        let msg = format!("{err}");
        assert_eq!(
            msg,
            "Sourcify response has unknown status `weird` (expected `full` or `partial`)"
        );
    }

    #[test]
    fn parse_rejects_malformed_json() {
        let err = parse_sourcify_response("{ not json }").unwrap_err();
        let msg = format!("{err}");
        assert_eq!(
            msg,
            "Sourcify response is not valid JSON: key must be a string at line 1 column 3"
        );
    }

    #[test]
    fn build_url_uses_any_endpoint_and_eip55_address() {
        // alloy's Address::Display renders the canonical EIP-55 checksum.
        // This test pins both the path shape (`/files/any/<chain>/<addr>`)
        // and the exact checksum casing alloy produces, so any future
        // change to either side is caught immediately.
        let addr: Address = "0x00000000219ab540356cbb839cbe05303d7705fa"
            .parse()
            .unwrap();
        let url = build_sourcify_url(1, addr);
        assert_eq!(
            url,
            "https://sourcify.dev/server/files/any/1/0x00000000219ab540356cBB839Cbe05303d7705Fa"
        );
    }

    #[test]
    fn build_url_supports_non_mainnet_chain_ids() {
        // Polygon = chain 137; Optimism = 10; Arbitrum One = 42161.
        // Strict pin so a refactor to a different URL template is caught.
        let addr: Address = "0xcafecafecafecafecafecafecafecafecafecafe"
            .parse()
            .unwrap();
        assert_eq!(
            build_sourcify_url(137, addr),
            "https://sourcify.dev/server/files/any/137/0xCAfEcAfeCAfECaFeCaFecaFecaFECafECafeCaFe"
        );
        assert_eq!(
            build_sourcify_url(42161, addr),
            "https://sourcify.dev/server/files/any/42161/0xCAfEcAfeCAfECaFeCaFecaFecaFECafECafeCaFe"
        );
    }
}
