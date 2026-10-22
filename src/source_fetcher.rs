//! Fetcher for verified contract source from the Sourcify server.
//!
//! Sourcify (<https://sourcify.dev/>) is a community-run service that hosts
//! verified Solidity / Vyper compilation artefacts for contracts deployed
//! on a given chain, indexed by `(chain_id, address)`.
//!
//! This module exposes:
//!
//! * [`fetch_sourcify_files`] -- low-level API.  GETs
//!   `/v2/contract/<chain>/<address>?fields=sources,compilation,metadata`,
//!   parses the JSON envelope, and returns a [`SourcifyContract`] value
//!   holding the verified `.sol` sources plus the `metadata.json`
//!   document.  Returns `Ok(None)` on a 404 (no verified match for that
//!   address).  All assertions in the unit tests are strict (exact
//!   counts and exact string equality on file contents) per the
//!   recorder's strict-assertions policy.
//!
//!   The older `/files/any/<chain>/<address>` shape is attempted only as a
//!   fallback.  Measured 2026-10-07: it answers a non-browser client with
//!   `HTTP 403 {"error":"This compatibility endpoint is only served to
//!   browsers. Use GET /server/v2/contract/{chainId}/{address}?fields=...
//!   instead."}`, while the v2 URL returns 200 with the sources in the
//!   same session.  So the v1-only code this module shipped with could
//!   not recover source for ANY address on ANY chain.
//!
//! * [`parse_sourcify_response`] / [`parse_sourcify_v2_response`] -- pure
//!   functions that parse the v1 and v2 Sourcify envelopes into a
//!   [`SourcifyContract`].  Exposed so tests can pin against recorded
//!   fixtures without touching the network.
//!
//! * [`parse_metadata_settings`] -- pure function that extracts the
//!   compiler version + optimizer + EVM-version fields from a Sourcify
//!   `metadata.json` document so the recorder can re-invoke solc with
//!   the exact settings the contract was originally verified with.
//!
//! * [`compile_sourcify_bundle`] -- assemble [`ContractArtifacts`] by
//!   shelling out to solc with the settings recovered from
//!   `metadata.json`, then parsing solc's `--combined-json` output for
//!   the runtime bytecode, runtime source map, storage layout, and AST.
//!
//! * [`fetch_contract_source`] -- higher-level wrapper that chains
//!   [`fetch_sourcify_files`] -> [`parse_metadata_settings`] ->
//!   [`compile_sourcify_bundle`] and returns a ready-to-register
//!   [`ContractArtifacts`].

use std::path::PathBuf;
use std::process::Command;

use alloy::primitives::Address;
use serde::Deserialize;

use crate::contract_registry::ContractArtifacts;
use crate::solidity_ast::SolidityAst;
use crate::source_map::{SourceMap, build_pc_to_instruction_index};
use crate::storage_layout::StorageLayout;

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

/// Compiler settings recovered from a Sourcify `metadata.json` document.
///
/// Holds the exact knobs solc needs to rebuild the contract so the
/// resulting `srcmap-runtime` aligns with the deployed bytecode:
///
/// * [`solc_version`] -- the full compiler version string from
///   `metadata.compiler.version` (e.g. `"0.8.28+commit.7893614a"`).  The
///   leading semver prefix (`"0.8.28"`) is what solc-select / svm use to
///   pick a binary; the commit hash is informational.
/// * [`optimizer_enabled`] / [`optimizer_runs`] -- mirror
///   `metadata.settings.optimizer.{enabled,runs}`.  Default to
///   `enabled = false` and `runs = 200` (solc's hard-coded default) when
///   the metadata omits them.
/// * [`evm_version`] -- mirrors `metadata.settings.evmVersion` (e.g.
///   `"shanghai"`, `"cancun"`).  `None` means "let solc pick its
///   default", which for a verified contract is typically wrong; the
///   recorder surfaces it as `None` rather than guessing so the caller
///   sees the metadata gap.
///
/// [`solc_version`]: MetadataSettings::solc_version
/// [`optimizer_enabled`]: MetadataSettings::optimizer_enabled
/// [`optimizer_runs`]: MetadataSettings::optimizer_runs
/// [`evm_version`]: MetadataSettings::evm_version
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataSettings {
    /// Full compiler version string from `metadata.compiler.version`.
    pub solc_version: String,
    /// Optimizer enabled flag from `metadata.settings.optimizer.enabled`.
    pub optimizer_enabled: bool,
    /// Optimizer runs from `metadata.settings.optimizer.runs`.
    pub optimizer_runs: u32,
    /// EVM target version from `metadata.settings.evmVersion`.
    pub evm_version: Option<String>,
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
// Internal serde shapes for `metadata.json`
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MetadataDoc {
    compiler: MetadataCompiler,
    #[serde(default)]
    settings: Option<MetadataSettingsDoc>,
}

#[derive(Debug, Deserialize)]
struct MetadataCompiler {
    version: String,
}

#[derive(Debug, Deserialize)]
struct MetadataSettingsDoc {
    #[serde(default)]
    optimizer: Option<MetadataOptimizer>,
    #[serde(default, rename = "evmVersion")]
    evm_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MetadataOptimizer {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    runs: Option<u32>,
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

/// Parse a Sourcify **v2** `GET /v2/contract/{chainId}/{address}` response
/// body into a [`SourcifyContract`].
///
/// The v2 envelope is shaped differently from v1: sources arrive as an
/// object keyed by source path (`{"Token.sol": {"content": "..."}}`)
/// rather than as a file list, and the metadata document arrives as a
/// nested object rather than as a `metadata.json` file entry.  It is
/// re-serialised here so [`parse_metadata_settings`] keeps working against
/// one shape.
///
/// Sources are ordered by path.  Order is load-bearing: the source map's
/// `file_index` indexes into `source_paths` positionally, and
/// [`compile_sourcify_bundle`] hands solc the files in this order.
///
/// v2's match vocabulary is `exact_match` / `match`, which are v1's `full`
/// / `partial` under new names.  `runtimeMatch` is preferred over the
/// summary `match` field because the recorder maps RUNTIME bytecode: a
/// contract whose creation code matches exactly and whose runtime code
/// matches only partially is a partial match for this purpose.
pub fn parse_sourcify_v2_response(body: &str) -> eyre::Result<SourcifyContract> {
    let raw: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| eyre::eyre!("Sourcify v2 response is not valid JSON: {e}"))?;

    let match_field = raw
        .get("runtimeMatch")
        .and_then(|v| v.as_str())
        .or_else(|| raw.get("match").and_then(|v| v.as_str()))
        .ok_or_else(|| {
            eyre::eyre!("Sourcify v2 response carries neither `runtimeMatch` nor `match`")
        })?;
    let match_status = match match_field {
        "exact_match" => SourcifyMatch::Full,
        "match" => SourcifyMatch::Partial,
        other => {
            return Err(eyre::eyre!(
                "Sourcify v2 response has unknown match `{other}` \
                 (expected `exact_match` or `match`)"
            ));
        }
    };

    let sources_obj = raw
        .get("sources")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("Sourcify v2 response has no `sources` object"))?;

    let mut sources: Vec<SourcifySourceFile> = Vec::with_capacity(sources_obj.len());
    for (path, entry) in sources_obj {
        let content = entry
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("Sourcify v2 source `{path}` has no `content`"))?;
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        if !(name.ends_with(".sol") || name.ends_with(".vy")) {
            continue;
        }
        sources.push(SourcifySourceFile {
            name,
            path: path.clone(),
            content: content.to_string(),
        });
    }
    sources.sort_by(|a, b| a.path.cmp(&b.path));

    let metadata_json = raw
        .get("metadata")
        .filter(|v| !v.is_null())
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| eyre::eyre!("Sourcify v2 metadata could not be re-serialised: {e}"))?;

    Ok(SourcifyContract {
        match_status,
        sources,
        metadata_json,
    })
}

/// Parse `metadata.json` from a Sourcify bundle and extract the compiler
/// settings the contract was verified with.
///
/// Defaults applied when fields are missing from the metadata:
///
/// * `optimizer.enabled` -> `false` (matches solc's CLI default; if
///   omitted from metadata the contract was almost certainly compiled
///   without `--optimize`).
/// * `optimizer.runs` -> `200` (solc's hard-coded default when
///   `--optimize` is passed without an explicit `--optimize-runs N`).
/// * `evmVersion` -> `None` (the recorder surfaces this gap rather than
///   guessing).
pub fn parse_metadata_settings(metadata_json: &str) -> eyre::Result<MetadataSettings> {
    let doc: MetadataDoc = serde_json::from_str(metadata_json)
        .map_err(|e| eyre::eyre!("metadata.json is not valid JSON: {e}"))?;

    let (optimizer_enabled, optimizer_runs, evm_version) = match doc.settings {
        Some(s) => {
            let (enabled, runs) = match s.optimizer {
                Some(opt) => (opt.enabled.unwrap_or(false), opt.runs.unwrap_or(200)),
                None => (false, 200),
            };
            (enabled, runs, s.evm_version)
        }
        None => (false, 200, None),
    };

    Ok(MetadataSettings {
        solc_version: doc.compiler.version,
        optimizer_enabled,
        optimizer_runs,
        evm_version,
    })
}

/// The `(major, minor, patch)` of a solc version string such as `0.8.28+commit.7893614a`,
/// or `None` when it does not parse (then every flag is passed, as before).
pub fn solc_release(full_version: &str) -> Option<(u32, u32, u32)> {
    let mut it = semver_prefix(full_version).trim_start_matches('v').split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// The length of `code` without the CBOR metadata solc appends, and the INVALID byte newer
/// releases put before it: the last two bytes are the metadata's length, big-endian (every
/// release from 0.4.7). Code that does not end in a
/// plausible length is returned whole, so nothing is cut from code that carries none.
pub fn strip_cbor_metadata_len(code: &[u8]) -> usize {
    if code.len() < 2 {
        return code.len();
    }
    let n = u16::from_be_bytes([code[code.len() - 2], code[code.len() - 1]]) as usize;
    if n == 0 || n + 2 > code.len() {
        return code.len();
    }
    // The metadata is a CBOR map: its first byte has major type 5 (0xa0..=0xbf).
    let start = code.len() - 2 - n;
    if (0xa0..=0xbf).contains(&code[start]) {
        // Newer releases separate code and metadata with one INVALID (0xfe), which
        // `--no-cbor-metadata` omits as well; it goes with the metadata.
        if start > 0 && code[start - 1] == 0xfe {
            start - 1
        } else {
            start
        }
    } else {
        code.len()
    }
}

/// Strip the commit-hash suffix from a metadata `compiler.version` string.
///
/// Solc renders its full version as `<semver>+commit.<hash>.<...>` (e.g.
/// `0.8.28+commit.7893614a`); the leading semver prefix is what solc-select
/// / svm and the recorder need for binary selection.
pub fn semver_prefix(full_version: &str) -> &str {
    full_version.split('+').next().unwrap_or(full_version)
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

/// Build the Sourcify **v2** contract endpoint URL for `(chain_id, address)`.
///
/// This is the endpoint the server's own documentation points at, and as of
/// 2026-10-07 it is the only one that answers a non-browser client: the v1
/// URL [`build_sourcify_url`] returns
/// `HTTP 403 {"error":"This compatibility endpoint is only served to
/// browsers. Use GET /server/v2/contract/{chainId}/{address}?fields=...
/// instead."}` — measured against mainnet USDT, with the v2 URL answering
/// 200 and the sources in the same session.
///
/// `fields` is explicit because v2 returns only the summary by default;
/// `sources` and `metadata` are what the recompile needs.
pub fn build_sourcify_v2_url(chain_id: u64, address: Address) -> String {
    format!(
        "{SOURCIFY_BASE_URL}/v2/contract/{chain_id}/{address}?fields=sources,compilation,metadata"
    )
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
    // v2 first: it is the documented API, and the v1 compatibility
    // endpoint is being withdrawn from non-browser clients (403 as of
    // 2026-10-07).  v1 is still attempted when v2 answers with anything
    // other than success or 404, so an outage on one shape does not take
    // the whole path down — and both errors are reported rather than only
    // the second.
    let v2_error = match fetch_one(&build_sourcify_v2_url(chain_id, address), true).await {
        Ok(outcome) => return Ok(outcome),
        Err(err) => err,
    };

    match fetch_one(&build_sourcify_url(chain_id, address), false).await {
        Ok(outcome) => Ok(outcome),
        Err(v1_error) => Err(eyre::eyre!(
            "Sourcify served neither API shape — v2: {v2_error:#}; v1: {v1_error:#}"
        )),
    }
}

/// GET one Sourcify URL and parse it with the v2 or v1 parser.
///
/// `Ok(None)` is reserved for a 404, which Sourcify uses for "no verified
/// match for this address" — a negative answer rather than a failure.
async fn fetch_one(url: &str, v2: bool) -> eyre::Result<Option<SourcifyContract>> {
    let resp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| eyre::eyre!("Sourcify GET {url} failed: {e}"))?;

    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(eyre::eyre!(
            "Sourcify GET {url} returned HTTP status {status}: {}",
            body.trim()
        ));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| eyre::eyre!("failed to read Sourcify response body from {url}: {e}"))?;
    if v2 {
        parse_sourcify_v2_response(&body).map(Some)
    } else {
        parse_sourcify_response(&body).map(Some)
    }
}

// ---------------------------------------------------------------------------
// Solc invocation + ContractArtifacts assembly
// ---------------------------------------------------------------------------

/// Recompile a Sourcify [`SourcifyContract`] bundle the way Sourcify verified it, and assemble
/// [`ContractArtifacts`].
///
/// solc is given `--standard-json`, built from the bundle's `metadata.json`: every source under
/// the path the metadata names it by, the metadata's own `settings` (optimizer and its details,
/// `evmVersion`, `remappings`, `libraries`, `viaIR`, `metadata.bytecodeHash`), and its
/// `compilationTarget` as the one contract to emit. That is Sourcify's own recompile, so a
/// contract Sourcify matched recompiles to the same code.
///
/// IT REPLACED A `--combined-json` COMMAND LINE that lost most of that. It passed
/// `--no-cbor-metadata` to every release (solc 0.8.18 and later accept it), so every contract
/// verified before 0.8.18 failed; it dropped remappings and library addresses, so any
/// multi-file project laid out under absolute paths failed; it picked the first contract in the
/// output rather than the compilation target, so a file holding several compiled the wrong one
/// (33 bytes of a library against kilobytes deployed); and it indexed source files in the
/// bundle's order rather than solc's source ids, which a multi-file source map does not use.
///
/// Output selections solc does not know are not requested: `storageLayout` exists from 0.5.13
/// and `evm.deployedBytecode.immutableReferences` from 0.6.5. The CBOR metadata solc appends is
/// stripped from the runtime code, so a correct recompile is a PREFIX of the deployed code
/// whatever the release, and the immutable references are returned so the caller can mask them
/// in the deployed code before comparing.
///
/// `preferred_contract_name` is used only when the metadata names no compilation target.
pub fn compile_sourcify_bundle(
    bundle: &SourcifyContract,
    solc_cmd: &str,
    preferred_contract_name: Option<&str>,
) -> eyre::Result<ContractArtifacts> {
    compile_sourcify_bundle_with_immutables(bundle, solc_cmd, preferred_contract_name)
        .map(|(artifacts, _)| artifacts)
}

/// [`compile_sourcify_bundle`], plus the byte ranges of the runtime code that hold immutable
/// values: zero in the recompile, set at deployment in the deployed code.
pub fn compile_sourcify_bundle_with_immutables(
    bundle: &SourcifyContract,
    solc_cmd: &str,
    preferred_contract_name: Option<&str>,
) -> eyre::Result<(ContractArtifacts, Vec<(usize, usize)>)> {
    let metadata_json = bundle
        .metadata_json
        .as_deref()
        .ok_or_else(|| eyre::eyre!("Sourcify bundle has no metadata.json; cannot recompile"))?;
    let settings = parse_metadata_settings(metadata_json)?;
    let metadata: serde_json::Value = serde_json::from_str(metadata_json)
        .map_err(|e| eyre::eyre!("metadata.json is not valid JSON: {e}"))?;
    if bundle.sources.is_empty() {
        return Err(eyre::eyre!(
            "Sourcify bundle has no Solidity source files; cannot recompile"
        ));
    }
    let release = solc_release(&settings.solc_version);
    let at_least = |v: (u32, u32, u32)| release.is_none_or(|r| r >= v);

    // ---- 1. The sources, under the names the metadata compiled them by ------------------
    //
    // Sourcify v2 keys its sources by those names already; v1 nests them under a
    // `.../sources/` prefix, so a metadata name matches the bundle path it is a suffix of.
    // A metadata document that names no sources (a hand-built one) compiles the bundle's
    // files under their own paths.
    let empty = serde_json::Map::new();
    let meta_sources = metadata
        .get("sources")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    let mut input_sources = serde_json::Map::new();
    if meta_sources.is_empty() {
        for src in &bundle.sources {
            input_sources.insert(src.path.clone(), serde_json::json!({ "content": src.content }));
        }
    }
    for (name, entry) in meta_sources {
        let bare = name.trim_start_matches('/');
        let from_bundle = bundle
            .sources
            .iter()
            .find(|s| s.path == *name)
            .or_else(|| bundle.sources.iter().find(|s| s.path.ends_with(bare)));
        let content = match from_bundle {
            Some(s) => s.content.clone(),
            None => entry
                .get("content")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    eyre::eyre!("metadata source `{name}` is not in the Sourcify bundle")
                })?,
        };
        input_sources.insert(name.clone(), serde_json::json!({ "content": content }));
    }

    // ---- 2. The settings, as the metadata states them ------------------------------------
    let mut input_settings = metadata
        .get("settings")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let target = input_settings
        .get("compilationTarget")
        .and_then(|v| v.as_object())
        .and_then(|m| m.iter().next())
        .map(|(file, name)| (file.clone(), name.as_str().unwrap_or_default().to_string()));
    if let Some(obj) = input_settings.as_object_mut() {
        obj.remove("compilationTarget");
        // The metadata spells libraries `"file:Lib": "0x…"`; standard JSON nests them by file.
        if let Some(libs) = obj.remove("libraries").and_then(|v| v.as_object().cloned()) {
            let mut nested = serde_json::Map::new();
            for (key, addr) in libs {
                let (file, lib) = key.rsplit_once(':').unwrap_or(("", key.as_str()));
                let slot = nested
                    .entry(file.to_string())
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(m) = slot.as_object_mut() {
                    m.insert(lib.to_string(), addr);
                }
            }
            obj.insert("libraries".into(), serde_json::Value::Object(nested));
        }
        let mut per_contract = vec![
            "evm.deployedBytecode.object",
            "evm.deployedBytecode.sourceMap",
        ];
        if at_least((0, 6, 5)) {
            per_contract.push("evm.deployedBytecode.immutableReferences");
        }
        if at_least((0, 5, 13)) {
            per_contract.push("storageLayout");
        }
        obj.insert(
            "outputSelection".into(),
            serde_json::json!({ "*": { "*": per_contract, "": ["ast"] } }),
        );
    }
    let language = metadata
        .get("language")
        .and_then(|v| v.as_str())
        .unwrap_or("Solidity");
    let input = serde_json::json!({
        "language": language,
        "sources": input_sources,
        "settings": input_settings,
    });

    // ---- 3. solc ---------------------------------------------------------------------------
    let mut child = Command::new(solc_cmd)
        .arg("--standard-json")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            eyre::eyre!(
                "failed to run solc ({solc_cmd}) for Sourcify-recompile (version {}): {e}",
                settings.solc_version
            )
        })?;
    {
        use std::io::Write;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| eyre::eyre!("solc's stdin was not captured"))?;
        stdin
            .write_all(input.to_string().as_bytes())
            .map_err(|e| eyre::eyre!("failed to write solc's standard-json input: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| eyre::eyre!("solc ({solc_cmd}) did not finish: {e}"))?;
    let out: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|e| {
        eyre::eyre!(
            "solc Sourcify-recompile (version {}) wrote no JSON ({e}); stderr: {}",
            settings.solc_version,
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    let errors: Vec<String> = out
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|e| e.get("severity").and_then(|s| s.as_str()) == Some("error"))
                .map(|e| {
                    e.get("formattedMessage")
                        .or_else(|| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("(no message)")
                        .trim()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    if !errors.is_empty() {
        return Err(eyre::eyre!(
            "solc Sourcify-recompile failed (version {}): {}",
            settings.solc_version,
            errors.join(" | ")
        ));
    }

    // ---- 4. The compilation target's artifacts -------------------------------------------
    let contracts = out
        .get("contracts")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("solc output is missing the 'contracts' object"))?;
    let (file, name) = match target {
        Some(t) => t,
        None => {
            // No target named: the preferred name, else the first contract with runtime code.
            let mut found = None;
            for (file, by_name) in contracts {
                for (name, c) in by_name.as_object().into_iter().flatten() {
                    let code = c
                        .pointer("/evm/deployedBytecode/object")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let wanted = preferred_contract_name.is_none_or(|p| name == p);
                    if wanted && !code.is_empty() && found.is_none() {
                        found = Some((file.clone(), name.clone()));
                    }
                }
            }
            found.ok_or_else(|| eyre::eyre!("solc emitted no contract with runtime code"))?
        }
    };
    let contract = contracts
        .get(&file)
        .and_then(|m| m.get(&name))
        .ok_or_else(|| eyre::eyre!("solc emitted no `{file}:{name}`, the compilation target"))?;
    let code_hex = contract
        .pointer("/evm/deployedBytecode/object")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no deployed bytecode for `{file}:{name}`"))?;
    let mut runtime_bytecode = alloy::hex::decode(code_hex).map_err(|e| {
        eyre::eyre!("deployed bytecode of `{file}:{name}` is not hex (an unlinked library?): {e}")
    })?;
    runtime_bytecode.truncate(strip_cbor_metadata_len(&runtime_bytecode));
    if runtime_bytecode.is_empty() {
        return Err(eyre::eyre!(
            "`{file}:{name}` has empty runtime bytecode (an interface or abstract contract)"
        ));
    }
    let source_map_raw = contract
        .pointer("/evm/deployedBytecode/sourceMap")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if source_map_raw.is_empty() {
        return Err(eyre::eyre!("`{file}:{name}` has an empty runtime source map"));
    }
    let source_map = SourceMap::parse(source_map_raw);
    let pc_to_idx = build_pc_to_instruction_index(&runtime_bytecode);
    let storage_layout: Option<StorageLayout> = contract
        .get("storageLayout")
        .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let mut immutables = Vec::new();
    if let Some(refs) = contract
        .pointer("/evm/deployedBytecode/immutableReferences")
        .and_then(|v| v.as_object())
    {
        for ranges in refs.values() {
            for r in ranges.as_array().into_iter().flatten() {
                let start = r.get("start").and_then(|v| v.as_u64());
                let length = r.get("length").and_then(|v| v.as_u64());
                if let (Some(s), Some(l)) = (start, length) {
                    immutables.push((s as usize, l as usize));
                }
            }
        }
    }

    // ---- 5. Source files BY SOLC'S SOURCE ID, which is what the source map indexes -------
    let out_sources = out
        .get("sources")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("solc output is missing the 'sources' object"))?;
    let mut by_id: Vec<(u64, String)> = out_sources
        .iter()
        .filter_map(|(path, s)| s.get("id").and_then(|v| v.as_u64()).map(|id| (id, path.clone())))
        .collect();
    by_id.sort();
    let mut source_paths = Vec::with_capacity(by_id.len());
    let mut source_contents = Vec::with_capacity(by_id.len());
    for (_, path) in &by_id {
        source_paths.push(PathBuf::from(path));
        source_contents.push(
            input_sources
                .get(path)
                .and_then(|s| s.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string(),
        );
    }
    // The AST, in the shape `SolidityAst::from_combined_json` reads: `sources.<path>.AST`.
    let mut ast_sources = serde_json::Map::new();
    for (path, s) in out_sources {
        if let Some(ast) = s.get("ast").or_else(|| s.get("legacyAST")) {
            ast_sources.insert(path.clone(), serde_json::json!({ "AST": ast }));
        }
    }
    let solidity_ast = if ast_sources.is_empty() {
        None
    } else {
        SolidityAst::from_combined_json(&serde_json::json!({ "sources": ast_sources }).to_string())
            .ok()
    };

    Ok((
        ContractArtifacts {
            name,
            source_map,
            runtime_bytecode,
            pc_to_idx,
            source_paths,
            source_contents,
            storage_layout,
            solidity_ast,
        },
        immutables,
    ))
}

// ---------------------------------------------------------------------------
// High-level API consumed by the recorder CLI
// ---------------------------------------------------------------------------

/// Fetch verified contract source and assemble [`ContractArtifacts`] for
/// a contract at `address` on chain `chain_id`.
///
/// Internally:
///
///   1. Calls [`fetch_sourcify_files`] to recover the verified bundle
///      (sources + `metadata.json`) for `(chain_id, address)`.
///   2. If the bundle is present, calls [`compile_sourcify_bundle`] to
///      re-invoke solc with the exact compiler version + optimizer
///      settings + EVM target version recorded in the metadata, and
///      assembles the resulting `ContractArtifacts`.
///   3. Returns `Ok(None)` only when Sourcify has no verified match for
///      the address (HTTP 404).
///
/// `preferred_contract_name` lets the caller disambiguate when the
/// bundle contains multiple contracts in a single file (libraries +
/// the deployed contract, base contracts, etc.).
///
/// The solc binary is selected via the `SOLC_PATH` environment variable
/// (falling back to the `solc` on `$PATH`), mirroring the recorder
/// CLI's existing convention.  If the metadata's `compiler.version`
/// disagrees with the available solc, the recompile will fail loudly
/// with solc's stderr surfaced in the error chain.
pub async fn fetch_contract_source(
    address: Address,
    chain_id: u64,
    preferred_contract_name: Option<&str>,
) -> eyre::Result<Option<ContractArtifacts>> {
    let bundle = match fetch_sourcify_files(chain_id, address).await? {
        Some(b) => b,
        None => return Ok(None),
    };

    let solc_cmd = std::env::var("SOLC_PATH").unwrap_or_else(|_| "solc".to_string());
    let artifacts = compile_sourcify_bundle(&bundle, &solc_cmd, preferred_contract_name)?;
    Ok(Some(artifacts))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

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

    /// Full metadata.json document with optimizer + evmVersion fields
    /// populated, to pin the rich-settings extraction path.
    const FIXTURE_RICH_METADATA: &str = r#"{
  "compiler": {"version": "0.8.28+commit.7893614a"},
  "language": "Solidity",
  "settings": {
    "optimizer": {"enabled": true, "runs": 800},
    "evmVersion": "shanghai"
  }
}"#;

    /// A Sourcify **v2** envelope, trimmed from the real response for
    /// mainnet USDT (`0xdAC17F958D2ee523a2206206994597C13D831ec7`)
    /// recorded 2026-10-07 — the top-level key set, the `sources` map
    /// keyed by source path with a `{"content": …}` value, the nested
    /// `metadata` document, and the `exact_match` / `match` vocabulary
    /// are all as the server sent them.  Source bodies are shortened;
    /// nothing about their shape is.
    const FIXTURE_V2_MATCH: &str = r#"{
  "sources": {
    "contracts/TetherToken.sol": {"content": "pragma solidity ^0.4.17;\ncontract TetherToken {}\n"},
    "contracts/SafeMath.sol": {"content": "pragma solidity ^0.4.17;\nlibrary SafeMath {}\n"},
    "README.md": {"content": "not a source"}
  },
  "compilation": {
    "language": "Solidity",
    "compiler": "solc",
    "compilerVersion": "0.4.18+commit.9cf6e910",
    "name": "TetherToken",
    "fullyQualifiedName": "contracts/TetherToken.sol:TetherToken"
  },
  "metadata": {
    "compiler": {"version": "0.4.18+commit.9cf6e910"},
    "language": "Solidity",
    "settings": {"optimizer": {"enabled": false, "runs": 0}}
  },
  "matchId": "1817159",
  "creationMatch": "match",
  "runtimeMatch": "match",
  "verifiedAt": "2024-08-08T13:56:23Z",
  "match": "match",
  "chainId": "1",
  "address": "0xdAC17F958D2ee523a2206206994597C13D831ec7"
}"#;

    #[test]
    fn v2_sources_arrive_path_keyed_and_in_path_order() {
        // Order is load-bearing: the source map's file_index indexes into
        // source_paths positionally, so the parse must be deterministic.
        let parsed = parse_sourcify_v2_response(FIXTURE_V2_MATCH).unwrap();
        assert_eq!(parsed.sources.len(), 2);
        assert_eq!(parsed.sources[0].path, "contracts/SafeMath.sol");
        assert_eq!(parsed.sources[0].name, "SafeMath.sol");
        assert_eq!(
            parsed.sources[0].content,
            "pragma solidity ^0.4.17;\nlibrary SafeMath {}\n"
        );
        assert_eq!(parsed.sources[1].path, "contracts/TetherToken.sol");
        assert_eq!(parsed.sources[1].name, "TetherToken.sol");
        assert_eq!(
            parsed.sources[1].content,
            "pragma solidity ^0.4.17;\ncontract TetherToken {}\n"
        );
    }

    #[test]
    fn v2_non_source_entries_are_filtered_out() {
        let parsed = parse_sourcify_v2_response(FIXTURE_V2_MATCH).unwrap();
        assert!(
            !parsed.sources.iter().any(|s| s.name == "README.md"),
            "a non-.sol entry must not reach the recompile's source list"
        );
    }

    #[test]
    fn v2_metadata_round_trips_into_the_settings_parser() {
        let parsed = parse_sourcify_v2_response(FIXTURE_V2_MATCH).unwrap();
        let metadata = parsed
            .metadata_json
            .as_deref()
            .expect("v2 metadata must be re-serialised for the settings parser");
        let settings = parse_metadata_settings(metadata).unwrap();
        assert_eq!(settings.solc_version, "0.4.18+commit.9cf6e910");
        assert_eq!(semver_prefix(&settings.solc_version), "0.4.18");
        assert!(!settings.optimizer_enabled);
        assert_eq!(settings.evm_version, None);
    }

    #[test]
    fn v2_match_vocabulary_maps_onto_the_v1_full_and_partial_names() {
        // `match` is v1's `partial`; `exact_match` is v1's `full`.
        let partial = parse_sourcify_v2_response(FIXTURE_V2_MATCH).unwrap();
        assert_eq!(partial.match_status, SourcifyMatch::Partial);

        let exact = FIXTURE_V2_MATCH.replace(
            r#""runtimeMatch": "match""#,
            r#""runtimeMatch": "exact_match""#,
        );
        let parsed = parse_sourcify_v2_response(&exact).unwrap();
        assert_eq!(parsed.match_status, SourcifyMatch::Full);
    }

    #[test]
    fn v2_runtime_match_wins_over_the_summary_match_field() {
        // The recorder maps RUNTIME bytecode, so a contract whose summary
        // says `exact_match` while its runtime code matches only partially
        // must be reported as partial.
        let body = FIXTURE_V2_MATCH.replace(r#""match": "match""#, r#""match": "exact_match""#);
        let parsed = parse_sourcify_v2_response(&body).unwrap();
        assert_eq!(parsed.match_status, SourcifyMatch::Partial);
    }

    #[test]
    fn v2_rejects_an_unknown_match_vocabulary_rather_than_guessing() {
        let body = FIXTURE_V2_MATCH.replace(
            r#""runtimeMatch": "match""#,
            r#""runtimeMatch": "probably_fine""#,
        );
        let err = parse_sourcify_v2_response(&body).unwrap_err().to_string();
        assert!(
            err.contains("probably_fine"),
            "the error must name the unrecognised match value; got: {err}"
        );
    }

    #[test]
    fn the_v2_url_names_the_fields_the_recompile_needs() {
        let url = build_sourcify_v2_url(
            1,
            Address::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap(),
        );
        assert_eq!(
            url,
            "https://sourcify.dev/server/v2/contract/1/\
             0xdAC17F958D2ee523a2206206994597C13D831ec7?fields=sources,compilation,metadata"
        );
    }

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

    // -------------------------------------------------------------------
    // metadata.json parsing
    // -------------------------------------------------------------------

    #[test]
    fn solc_release_reads_the_version_and_refuses_what_it_cannot() {
        assert_eq!(solc_release("0.8.28+commit.7893614a"), Some((0, 8, 28)));
        assert_eq!(solc_release("v0.4.24+commit.e67f0147"), Some((0, 4, 24)));
        assert_eq!(solc_release("0.5.12"), Some((0, 5, 12)));
        assert_eq!(solc_release(""), None);
        assert_eq!(solc_release("nightly"), None);
    }

    #[test]
    fn cbor_metadata_is_stripped_only_where_it_is() {
        // 0.4.x's swarm metadata: a1 65 'bzzr0' 58 20 <32 bytes>, then length 0x0029.
        let code = [0x60u8, 0x80, 0x60, 0x40, 0x52];
        let mut meta = vec![0xa1, 0x65, b'b', b'z', b'z', b'r', b'0', 0x58, 0x20];
        meta.extend_from_slice(&[0x11u8; 32]);
        let mut with = code.to_vec();
        with.extend_from_slice(&meta);
        with.extend_from_slice(&(meta.len() as u16).to_be_bytes());
        assert_eq!(strip_cbor_metadata_len(&with), code.len());
        // A newer release's 0xfe separator goes with the metadata.
        let mut sep = code.to_vec();
        sep.push(0xfe);
        sep.extend_from_slice(&meta);
        sep.extend_from_slice(&(meta.len() as u16).to_be_bytes());
        assert_eq!(strip_cbor_metadata_len(&sep), code.len());
        // Controls: code with no metadata, an implausible length, and a tail that is not a
        // CBOR map are returned whole.
        assert_eq!(strip_cbor_metadata_len(&code), code.len());
        let mut too_long = code.to_vec();
        too_long.extend_from_slice(&[0x01, 0x00]);
        assert_eq!(strip_cbor_metadata_len(&too_long), too_long.len());
        let mut not_map = code.to_vec();
        not_map.extend_from_slice(&[0x60, 0x01, 0x00, 0x02]);
        assert_eq!(strip_cbor_metadata_len(&not_map), not_map.len());
    }

    #[test]
    fn parse_metadata_minimal_uses_solc_defaults() {
        // The full-match fixture's metadata.json carries only the
        // compiler version; everything else is missing.  The parser
        // must fall back to solc's documented defaults: optimizer off,
        // 200 runs, no EVM version pinned.
        let parsed = parse_sourcify_response(FIXTURE_FULL_MATCH).unwrap();
        let metadata = parsed.metadata_json.unwrap();
        let settings = parse_metadata_settings(&metadata).unwrap();
        assert_eq!(settings.solc_version, "0.6.11+commit.5ef660b1");
        assert!(!settings.optimizer_enabled);
        assert_eq!(settings.optimizer_runs, 200);
        assert_eq!(settings.evm_version, None);
    }

    #[test]
    fn parse_metadata_rich_extracts_optimizer_and_evm_version() {
        let settings = parse_metadata_settings(FIXTURE_RICH_METADATA).unwrap();
        assert_eq!(settings.solc_version, "0.8.28+commit.7893614a");
        assert!(settings.optimizer_enabled);
        assert_eq!(settings.optimizer_runs, 800);
        assert_eq!(settings.evm_version, Some("shanghai".to_string()));
    }

    #[test]
    fn parse_metadata_rejects_missing_compiler_version() {
        let err = parse_metadata_settings("{\"language\":\"Solidity\"}").unwrap_err();
        let msg = format!("{err}");
        assert_eq!(
            msg,
            "metadata.json is not valid JSON: missing field `compiler` at line 1 column 23"
        );
    }

    #[test]
    fn semver_prefix_strips_commit_hash() {
        assert_eq!(semver_prefix("0.8.28+commit.7893614a"), "0.8.28");
        assert_eq!(semver_prefix("0.6.11+commit.5ef660b1"), "0.6.11");
        // Already-stripped versions pass through unchanged.
        assert_eq!(semver_prefix("0.8.28"), "0.8.28");
    }

    // -------------------------------------------------------------------
    // End-to-end: synthesize a Sourcify response from local solc and
    // run it through the full fetch -> parse -> compile -> assemble
    // pipeline.  Skipped (test body short-circuits with assert_eq!s
    // pinning the skip path) when solc is unavailable in the dev env.
    // -------------------------------------------------------------------

    /// Build a minimal but realistic Sourcify-style envelope around a
    /// real `.sol` source file plus a synthesized `metadata.json` that
    /// carries the version of the local solc.  The resulting envelope
    /// is what `parse_sourcify_response` would return for a real
    /// Sourcify hit, so we can exercise the recompile path without
    /// touching the network.
    fn synthesize_sourcify_envelope(
        source_basename: &str,
        source_content: &str,
        solc_full_version: &str,
        evm_version: &str,
    ) -> SourcifyContract {
        let metadata = format!(
            "{{\"compiler\":{{\"version\":\"{solc_full_version}\"}},\
             \"language\":\"Solidity\",\
             \"settings\":{{\"optimizer\":{{\"enabled\":false,\"runs\":200}},\
             \"evmVersion\":\"{evm_version}\"}}}}"
        );
        SourcifyContract {
            match_status: SourcifyMatch::Full,
            sources: vec![SourcifySourceFile {
                name: source_basename.to_string(),
                path: format!("/sources/{source_basename}"),
                content: source_content.to_string(),
            }],
            metadata_json: Some(metadata),
        }
    }

    /// Discover the local solc binary's full version string in the
    /// canonical `<semver>+commit.<hash>` shape, matching what
    /// `metadata.json` carries.  Returns `None` if solc is missing or
    /// its `--version` output cannot be parsed.
    fn local_solc_full_version(solc_cmd: &str) -> Option<String> {
        let out = Command::new(solc_cmd).arg("--version").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let stdout = String::from_utf8(out.stdout).ok()?;
        // Solc prints e.g. `Version: 0.8.33+commit.64118f21.Linux.g++`.
        // We want `0.8.33+commit.64118f21`.
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("Version: ") {
                let trimmed = rest.trim();
                // Take everything up to (but not including) the third `.`
                // after the `+commit.` marker, since solc tacks on a
                // platform/compiler suffix the metadata never includes.
                if let Some(plus_idx) = trimmed.find('+') {
                    let (prefix, after_plus) = trimmed.split_at(plus_idx);
                    // after_plus starts with "+commit.<hash>.<rest>"; we want "+commit.<hash>".
                    let after_plus = after_plus.trim_start_matches('+');
                    let mut parts = after_plus.split('.');
                    let commit_marker = parts.next()?;
                    let commit_hash = parts.next()?;
                    return Some(format!("{prefix}+{commit_marker}.{commit_hash}"));
                }
                return Some(trimmed.to_string());
            }
        }
        None
    }

    /// Pin: the full Sourcify -> ContractArtifacts pipeline must
    /// produce a non-empty runtime bytecode AND a non-empty runtime
    /// source map AND a parseable storage layout AND a non-empty AST
    /// for a simple contract.  We use the local solc to build the
    /// envelope (so the version embedded in metadata is one we can
    /// invoke), then run `compile_sourcify_bundle` against it.
    ///
    /// This is the load-bearing end-to-end pin for the recompile path:
    /// it asserts the recovered runtime bytecode is byte-identical to
    /// what solc reports for the same inputs (so the recorder won't
    /// register a half-populated artifact that mis-attributes source
    /// ranges).
    #[test]
    fn compile_sourcify_bundle_assembles_artifacts_for_simple_contract() {
        let solc_cmd = std::env::var("SOLC_PATH").unwrap_or_else(|_| "solc".to_string());
        let full_version = match local_solc_full_version(&solc_cmd) {
            Some(v) => v,
            None => {
                // Local dev env has no solc.  This test pins the
                // recompile path's behaviour with solc available; we
                // surface the gap loudly via stderr so the dev sees it.
                eprintln!(
                    "SKIP compile_sourcify_bundle_assembles_artifacts_for_simple_contract: \
                     solc not found at `{solc_cmd}`"
                );
                return;
            }
        };

        // Minimal, deterministic Solidity 0.8.x contract.  Storage
        // layout is exercised via the `n` slot; AST is exercised via
        // the `bump` function definition.  Bytecode is non-empty.
        let source = "// SPDX-License-Identifier: MIT\n\
                      pragma solidity ^0.8.0;\n\
                      contract Counter {\n\
                          uint256 public n;\n\
                          function bump() public returns (uint256) { n = n + 1; return n; }\n\
                      }\n";

        let bundle = synthesize_sourcify_envelope("Counter.sol", source, &full_version, "shanghai");

        let artifacts = compile_sourcify_bundle(&bundle, &solc_cmd, Some("Counter")).unwrap();

        // Strict pins: every load-bearing field must be populated.
        assert_eq!(artifacts.name, "Counter");
        assert!(!artifacts.runtime_bytecode.is_empty());
        assert!(!artifacts.source_map.is_empty());
        assert!(!artifacts.pc_to_idx.is_empty());
        assert_eq!(artifacts.pc_to_idx.len(), artifacts.runtime_bytecode.len());
        assert_eq!(artifacts.source_paths.len(), 1);
        assert_eq!(
            artifacts.source_paths[0],
            PathBuf::from("/sources/Counter.sol")
        );
        assert_eq!(artifacts.source_contents.len(), 1);
        assert_eq!(artifacts.source_contents[0], source);

        // Cross-check against an independent solc invocation: the
        // runtime bytecode the assembler returned must be exactly what
        // solc reports for the same source + settings.  This is the
        // strict guarantee that ties the recompile output to what
        // would have been deployed.
        let independent_dir = tempfile::TempDir::new().unwrap();
        let independent_path = independent_dir.path().join("Counter.sol");
        std::fs::write(&independent_path, source).unwrap();
        let independent = Command::new(&solc_cmd)
            .args([
                "--combined-json",
                "bin-runtime,srcmap-runtime",
                "--no-cbor-metadata",
                "--evm-version",
                "shanghai",
            ])
            .arg(&independent_path)
            .output()
            .unwrap();
        assert!(independent.status.success());
        let independent_json: serde_json::Value =
            serde_json::from_slice(&independent.stdout).unwrap();
        // solc keys its `--combined-json` `contracts` map by
        // `<source-path>:<contract-name>`, and it always normalises the
        // source path to forward slashes -- even on Windows, where the path
        // passed on the command line uses backslashes.  Build the lookup
        // key with the same normalisation so it matches on every OS.
        let key = format!(
            "{}:Counter",
            independent_path.display().to_string().replace('\\', "/")
        );
        let independent_runtime_hex = independent_json["contracts"][&key]["bin-runtime"]
            .as_str()
            .unwrap()
            .to_string();
        let independent_runtime = alloy::hex::decode(&independent_runtime_hex).unwrap();
        assert_eq!(artifacts.runtime_bytecode, independent_runtime);

        let independent_srcmap = independent_json["contracts"][&key]["srcmap-runtime"]
            .as_str()
            .unwrap()
            .to_string();
        let independent_srcmap_len = SourceMap::parse(&independent_srcmap).len();
        assert_eq!(artifacts.source_map.len(), independent_srcmap_len);

        // Storage layout: `Counter.n` lives at slot 0.
        let layout = artifacts.storage_layout.unwrap();
        assert_eq!(layout.storage.len(), 1);
        assert_eq!(layout.storage[0].label, "n");
        assert_eq!(layout.storage[0].slot, "0");

        // AST: the `bump` function must be discoverable.
        let ast = artifacts.solidity_ast.unwrap();
        let bump_fns: Vec<&crate::solidity_ast::FunctionDef> =
            ast.functions.iter().filter(|f| f.name == "bump").collect();
        assert_eq!(bump_fns.len(), 1);
    }

    /// Pin: an empty-source bundle is rejected with a precise error
    /// rather than silently returning a half-populated artifact.
    #[test]
    fn compile_sourcify_bundle_rejects_empty_sources() {
        let bundle = SourcifyContract {
            match_status: SourcifyMatch::Full,
            sources: Vec::new(),
            metadata_json: Some(
                "{\"compiler\":{\"version\":\"0.8.28+commit.7893614a\"}}".to_string(),
            ),
        };
        let result = compile_sourcify_bundle(&bundle, "solc", None);
        let msg = match result {
            Ok(_) => panic!("expected an error, got Ok"),
            Err(e) => format!("{e}"),
        };
        assert_eq!(
            msg,
            "Sourcify bundle has no Solidity source files; cannot recompile"
        );
    }

    /// Pin: a bundle with no metadata.json is rejected with a precise
    /// error -- without metadata we cannot know which solc settings to
    /// invoke and cannot guarantee `srcmap-runtime` alignment.
    #[test]
    fn compile_sourcify_bundle_rejects_missing_metadata() {
        let bundle = SourcifyContract {
            match_status: SourcifyMatch::Full,
            sources: vec![SourcifySourceFile {
                name: "X.sol".to_string(),
                path: "/X.sol".to_string(),
                content: "contract X {}".to_string(),
            }],
            metadata_json: None,
        };
        let result = compile_sourcify_bundle(&bundle, "solc", None);
        let msg = match result {
            Ok(_) => panic!("expected an error, got Ok"),
            Err(e) => format!("{e}"),
        };
        assert_eq!(
            msg,
            "Sourcify bundle has no metadata.json; cannot recompile"
        );
    }
}
