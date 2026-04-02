//! CLI entry point for the CodeTracer EVM recorder.
//!
//! Supports the `record` subcommand which compiles a Solidity file, deploys
//! it to a local Anvil node, calls a specified function, fetches the
//! `debug_traceTransaction` structlogs, processes them through the EVM
//! recorder pipeline, and writes the trace output files.
//!
//! # Usage
//!
//! ```text
//! codetracer-evm-recorder record <solidity-file> \
//!     --trace-dir <output-dir> \
//!     [--function <name>]
//! ```
//!
//! The `record` subcommand will:
//! 1. Compile `<solidity-file>` with `solc --combined-json`.
//! 2. Spin up a local Anvil node (with `--steps-tracing`).
//! 3. Deploy the first contract found in the compiled output.
//! 4. Call `--function` (defaults to `run`, falls back to the first
//!    non-constructor function in the ABI).
//! 5. Fetch `debug_traceTransaction` structlogs.
//! 6. Run the [`EvmRecorder`] pipeline.
//! 7. Write `trace.bin`, `trace_metadata.json`, and `trace_paths.json` into
//!    `--trace-dir`.
//! 8. Copy the source file into `--trace-dir` so the db-backend can resolve
//!    source paths when the trace is loaded.

use std::path::{Path, PathBuf};
use std::process::Command;

use alloy::network::TransactionBuilder;
use alloy::providers::Provider;
use alloy::providers::ProviderBuilder;
use clap::{Parser, Subcommand};
use eyre::{Context, Result};

use codetracer_evm_recorder::recorder::EvmRecorder;
use codetracer_evm_recorder::solidity_ast::SolidityAst;
use codetracer_evm_recorder::source_map::SourceMap;
use codetracer_evm_recorder::storage_layout::StorageLayout;
use codetracer_evm_recorder::trace_fetcher;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// CodeTracer EVM recorder — record Solidity/EVM execution traces.
#[derive(Debug, Parser)]
#[command(
    name = "codetracer-evm-recorder",
    version,
    about = "Record EVM smart-contract execution traces for CodeTracer"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Compile and record a Solidity source file.
    ///
    /// Compiles the contract with `solc`, deploys it to a temporary local
    /// Anvil node, calls the specified entry-point function, and writes the
    /// CodeTracer trace files to `--trace-dir`.
    Record(RecordArgs),
}

#[derive(Debug, clap::Args)]
struct RecordArgs {
    /// Path to the Solidity source file (.sol).
    solidity_file: PathBuf,

    /// Directory where the trace files will be written.
    ///
    /// The directory will be created if it does not exist.
    /// Three files are produced: `trace.bin`, `trace_metadata.json`, and
    /// `trace_paths.json`. The source file is also copied into this directory.
    #[arg(long)]
    trace_dir: PathBuf,

    /// Name of the function to call (without argument types or parentheses).
    ///
    /// Defaults to `run`. If a function named `run` does not exist in the
    /// ABI, the first non-constructor function is used instead.
    #[arg(long, default_value = "run")]
    function: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Record(args) => record(args).await,
    }
}

// ---------------------------------------------------------------------------
// `record` implementation
// ---------------------------------------------------------------------------

/// Execute the `record` subcommand.
async fn record(args: RecordArgs) -> Result<()> {
    let source_path = args
        .solidity_file
        .canonicalize()
        .with_context(|| format!("source file not found: {}", args.solidity_file.display()))?;

    let trace_dir = &args.trace_dir;
    std::fs::create_dir_all(trace_dir)
        .with_context(|| format!("cannot create trace dir: {}", trace_dir.display()))?;

    // -----------------------------------------------------------------------
    // 1. Compile the Solidity file with solc
    // -----------------------------------------------------------------------
    let solc_cmd = std::env::var("SOLC_PATH").unwrap_or_else(|_| "solc".to_string());
    let compile_output = Command::new(&solc_cmd)
        .args([
            "--combined-json",
            "abi,bin,bin-runtime,srcmap-runtime,storage-layout,ast",
            "--no-cbor-metadata",
            source_path.to_str().unwrap(),
        ])
        .output()
        .with_context(|| format!("failed to run solc ({})", solc_cmd))?;

    if !compile_output.status.success() {
        return Err(eyre::eyre!(
            "solc compilation failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&compile_output.stdout),
            String::from_utf8_lossy(&compile_output.stderr)
        ));
    }

    let compiled_json_str = String::from_utf8(compile_output.stdout.clone())
        .context("solc output is not valid UTF-8")?;
    let compiled: serde_json::Value =
        serde_json::from_str(&compiled_json_str).context("solc output is not valid JSON")?;

    // Parse the Solidity AST for local variable extraction.
    // The combined-json output includes AST data in a "sources" key that
    // SolidityAst::from_combined_json knows how to parse.
    let solidity_ast = SolidityAst::from_combined_json(&compiled_json_str)
        .context("failed to parse Solidity AST from solc output")?;

    // -----------------------------------------------------------------------
    // 2. Extract the first contract from the compiled output
    //
    // solc --combined-json keys look like "path/to/File.sol:ContractName".
    // We pick the contract whose name matches the stem of the source file
    // (common convention), or fall back to the first available contract.
    // -----------------------------------------------------------------------
    let contracts = compiled["contracts"]
        .as_object()
        .ok_or_else(|| eyre::eyre!("unexpected solc output: missing 'contracts' object"))?;

    let file_stem = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Contract");

    // Prefer a contract whose key ends with ":<FileStem>" (canonical convention).
    let preferred_suffix = format!(":{}", file_stem);
    let (contract_key, contract_json) = contracts
        .iter()
        .find(|(k, _)| k.ends_with(&preferred_suffix))
        .or_else(|| contracts.iter().next())
        .ok_or_else(|| eyre::eyre!("no contracts found in solc output"))?;

    // Extract the contract name for use as the recorder's program label.
    let contract_name = contract_key.split(':').next_back().unwrap_or(file_stem);

    let deploy_bytecode_hex = contract_json["bin"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("missing 'bin' in contract JSON for {}", contract_key))?;

    let runtime_bytecode_hex = contract_json["bin-runtime"].as_str().ok_or_else(|| {
        eyre::eyre!(
            "missing 'bin-runtime' in contract JSON for {}",
            contract_key
        )
    })?;

    let source_map_raw = contract_json["srcmap-runtime"].as_str().ok_or_else(|| {
        eyre::eyre!(
            "missing 'srcmap-runtime' in contract JSON for {}",
            contract_key
        )
    })?;

    let storage_layout_json = &contract_json["storage-layout"];

    // Parse source map
    let source_map = SourceMap::parse(source_map_raw);

    // Parse storage layout (may be absent for contracts with no storage)
    let storage_layout: Option<StorageLayout> = if storage_layout_json.is_null()
        || storage_layout_json.as_object().is_none_or(|o| o.is_empty())
    {
        None
    } else {
        serde_json::from_value(storage_layout_json.clone())
            .context("failed to parse storage-layout from solc output")
            .ok()
    };

    // Decode bytecode bytes
    let runtime_bytecode = alloy::hex::decode(runtime_bytecode_hex)
        .context("invalid runtime bytecode hex from solc")?;

    // Read the source file content for the recorder
    let source_contents = std::fs::read_to_string(&source_path)
        .with_context(|| format!("failed to read source file: {}", source_path.display()))?;

    // -----------------------------------------------------------------------
    // 3. Determine which function to call
    //
    // The ABI is an array of objects with "type" (function/constructor/event).
    // solc --combined-json outputs the ABI as a JSON array directly in the
    // value (not as a JSON-encoded string), so we accept both forms for
    // forward compatibility.
    // We look for a function matching `args.function`, then fall back to the
    // first non-constructor function.
    // -----------------------------------------------------------------------
    let abi_value = &contract_json["abi"];
    let abi: Vec<serde_json::Value> = if let Some(s) = abi_value.as_str() {
        // Older solc versions serialise the ABI as a JSON string inside the
        // combined-json output. Parse it.
        serde_json::from_str(s).context("failed to parse ABI JSON string")?
    } else if let Some(arr) = abi_value.as_array() {
        // Modern solc versions embed the ABI array directly.
        arr.clone()
    } else {
        return Err(eyre::eyre!(
            "missing or unexpected 'abi' format in contract JSON for {}",
            contract_key
        ));
    };

    let function_name = resolve_function_name(&abi, &args.function);
    eprintln!(
        "Calling function: {}() on contract {}",
        function_name, contract_name
    );

    // Build the function selector (keccak256 of "name()")[0..4]
    let selector_input = format!("{}()", function_name);
    let selector = &alloy::primitives::keccak256(selector_input.as_bytes())[..4];

    // Determine constructor arguments (encode them if needed)
    let constructor_args = encode_constructor_args(&abi, contract_name)?;

    // -----------------------------------------------------------------------
    // 4. Spin up a local Anvil node
    //
    // `--steps-tracing` is required for debug_traceTransaction to return
    // non-empty structLogs in Foundry/Anvil >= 1.5.0.
    // -----------------------------------------------------------------------
    let anvil = alloy::node_bindings::Anvil::new()
        .arg("--steps-tracing")
        .spawn();
    let rpc_url = anvil.endpoint();

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let accounts = provider.get_accounts().await?;
    let from = accounts[0];

    // -----------------------------------------------------------------------
    // 5. Deploy the contract
    // -----------------------------------------------------------------------
    let deploy_data = {
        let mut bytes = alloy::hex::decode(deploy_bytecode_hex)
            .context("invalid deploy bytecode hex from solc")?;
        // Append ABI-encoded constructor arguments if any
        bytes.extend_from_slice(&constructor_args);
        alloy::primitives::Bytes::from(bytes)
    };

    let deploy_tx = alloy::rpc::types::TransactionRequest::default()
        .from(from)
        .with_deploy_code(deploy_data);

    let deploy_pending = provider
        .send_transaction(deploy_tx)
        .await
        .context("failed to send deploy transaction")?;
    let deploy_receipt = deploy_pending
        .get_receipt()
        .await
        .context("failed to get deploy receipt")?;
    let contract_address = deploy_receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("deploy transaction produced no contract address"))?;

    eprintln!("Deployed {} at {}", contract_name, contract_address);

    // -----------------------------------------------------------------------
    // 6. Call the target function
    // -----------------------------------------------------------------------
    let call_tx = alloy::rpc::types::TransactionRequest::default()
        .from(from)
        .to(contract_address)
        .with_input(alloy::primitives::Bytes::copy_from_slice(selector));

    let call_pending = provider
        .send_transaction(call_tx)
        .await
        .context("failed to send function call transaction")?;
    let call_receipt = call_pending
        .get_receipt()
        .await
        .context("failed to get function call receipt")?;
    let tx_hash = call_receipt.transaction_hash;

    eprintln!("Transaction: {:?}", tx_hash);

    // -----------------------------------------------------------------------
    // 7. Fetch debug_traceTransaction structlogs
    // -----------------------------------------------------------------------
    let frame = trace_fetcher::fetch_struct_logs(&rpc_url, tx_hash)
        .await
        .context("failed to fetch structlogs via debug_traceTransaction")?;

    let struct_logs = trace_fetcher::extract_struct_logs(&frame);

    eprintln!("Fetched {} struct log entries", struct_logs.len());

    if struct_logs.is_empty() {
        return Err(eyre::eyre!(
            "debug_traceTransaction returned no structlogs — \
             ensure anvil was started with --steps-tracing"
        ));
    }

    // -----------------------------------------------------------------------
    // 8. Copy the source file into the trace directory
    //
    // The db-backend resolves source paths from trace_paths.json relative to
    // the trace workdir. By writing and copying the source file into
    // trace_dir we ensure the path remains valid even when the caller moves
    // the trace around.
    // -----------------------------------------------------------------------
    let source_filename = source_path
        .file_name()
        .ok_or_else(|| eyre::eyre!("source path has no filename component"))?;
    let source_copy_path = trace_dir.join(source_filename);
    std::fs::copy(&source_path, &source_copy_path).with_context(|| {
        format!(
            "failed to copy source file into trace dir: {} -> {}",
            source_path.display(),
            source_copy_path.display()
        )
    })?;

    // -----------------------------------------------------------------------
    // 9. Process through the recorder and write trace output
    // -----------------------------------------------------------------------
    let mut recorder =
        EvmRecorder::new(contract_name, trace_dir).context("failed to create EvmRecorder")?;
    recorder
        .initialize()
        .context("failed to initialize EvmRecorder")?;

    let source_path_ref: &Path = source_copy_path.as_path();
    recorder
        .record_from_structlog(
            struct_logs,
            &source_map,
            &runtime_bytecode,
            &[source_path_ref],
            &[source_contents.as_str()],
            storage_layout.as_ref(),
            Some(&solidity_ast),
        )
        .context("recorder failed to process structlogs")?;

    recorder
        .finalize()
        .context("failed to finalize EvmRecorder")?;

    eprintln!("Trace written to {}", trace_dir.display());
    eprintln!("  trace.json");
    eprintln!("  trace_metadata.json");
    eprintln!("  trace_paths.json");
    eprintln!("  {}", source_filename.to_string_lossy());

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the function name to call.
///
/// Looks for a function named `preferred` in the ABI; if not found, returns
/// the name of the first non-constructor function. Panics if no callable
/// function exists.
fn resolve_function_name(abi: &[serde_json::Value], preferred: &str) -> String {
    // Collect all callable functions (type == "function").
    let functions: Vec<&str> = abi
        .iter()
        .filter(|item| item.get("type").and_then(|v| v.as_str()) == Some("function"))
        .filter_map(|item| item.get("name").and_then(|v| v.as_str()))
        .collect();

    // Return the preferred function if present.
    if functions.contains(&preferred) {
        return preferred.to_string();
    }

    // Fall back to the first available function.
    functions
        .first()
        .map(|s| s.to_string())
        .unwrap_or_else(|| preferred.to_string())
}

/// Encode default constructor arguments for well-known patterns.
///
/// Solidity constructors with primitive arguments are ABI-encoded as packed
/// 32-byte big-endian integers. This function handles the common pattern of a
/// single `uint256` argument (e.g. `constructor(uint256 a)`) by providing a
/// sensible default value (`10`).
///
/// For constructors with no arguments or unknown/complex signatures, an empty
/// byte slice is returned and deployment proceeds without extra calldata.
///
/// # Limitations
///
/// Full ABI encoding for arbitrary constructor signatures is outside the scope
/// of the CLI. If the contract's constructor requires non-trivial arguments,
/// encoding will fail with a descriptive error.
fn encode_constructor_args(abi: &[serde_json::Value], contract_name: &str) -> Result<Vec<u8>> {
    // Find the constructor entry in the ABI, if any.
    let constructor = abi
        .iter()
        .find(|item| item.get("type").and_then(|v| v.as_str()) == Some("constructor"));

    let constructor = match constructor {
        None => return Ok(vec![]), // no constructor => no args needed
        Some(c) => c,
    };

    let inputs = constructor
        .get("inputs")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    if inputs.is_empty() {
        return Ok(vec![]); // default (no-arg) constructor
    }

    // For a single uint256 argument we use a default value of 10,
    // which matches the canonical flow-test contract pattern.
    if inputs.len() == 1 {
        let type_str = inputs[0].get("type").and_then(|v| v.as_str()).unwrap_or("");
        if type_str.starts_with("uint") {
            // ABI-encode a single uint256(10): 32-byte big-endian
            let mut encoded = vec![0u8; 32];
            encoded[31] = 10;
            return Ok(encoded);
        }
    }

    Err(eyre::eyre!(
        "constructor for {} has {} argument(s) with types that the CLI cannot \
         automatically encode. Please simplify the constructor or provide a \
         no-argument factory function.",
        contract_name,
        inputs.len()
    ))
}
