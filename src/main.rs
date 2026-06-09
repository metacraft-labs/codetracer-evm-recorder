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
//!     --out-dir <output-dir> \
//!     [--function <name>]
//! ```
//!
//! The recorder always writes a canonical CodeTracer multi-stream CTFS
//! `.ct` bundle (see `Recorder-CLI-Conventions.md` §4 in
//! `codetracer-specs`). Human-readable conversion of CTFS traces is the
//! job of `ct print` (shipped with `codetracer-trace-format-nim`).
//!
//! The `record` subcommand will:
//! 1. Compile `<solidity-file>` with `solc --combined-json`.
//! 2. Spin up a local Anvil node (with `--steps-tracing`).
//! 3. Deploy the first contract found in the compiled output.
//! 4. Call `--function` (defaults to `run`, falls back to the first
//!    non-constructor function in the ABI).
//! 5. Fetch `debug_traceTransaction` structlogs.
//! 6. Run the [`EvmRecorder`] pipeline.
//! 7. Write the CTFS bundle into `--out-dir`.
//! 8. Copy the source file into `--out-dir` so the db-backend can resolve
//!    source paths when the trace is loaded.
//!
//! # Environment variables
//!
//! * `CODETRACER_EVM_RECORDER_OUT_DIR` — fallback for `--out-dir` when the
//!   flag is not given. The CLI flag always wins.
//! * `CODETRACER_EVM_RECORDER_DISABLED` — set to `1` or `true` to skip
//!   recording entirely. The recorder still validates inputs but does not
//!   spin up Anvil or write any trace artefacts.
//!
//! # Deprecated flags
//!
//! * `--trace-dir` — legacy alias for `--out-dir`. Still accepted (so
//!   existing scripts keep working) but emits a one-line stderr
//!   deprecation note. New callers should use `--out-dir` / `-o`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use alloy::network::TransactionBuilder;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::providers::ProviderBuilder;
use clap::{Parser, Subcommand};
use eyre::{Context, Result};

use codetracer_evm_recorder::recorder::EvmRecorder;
use codetracer_evm_recorder::revert_decode;
use codetracer_evm_recorder::solidity_ast::SolidityAst;
use codetracer_evm_recorder::source_map::SourceMap;
use codetracer_evm_recorder::storage_layout::StorageLayout;
use codetracer_evm_recorder::trace_fetcher;
use codetracer_evm_recorder::yul_compile;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Environment variable used as a fallback for `--out-dir` when the CLI
/// flag is omitted.  Convention: see `Recorder-CLI-Conventions.md` §5.
const ENV_OUT_DIR: &str = "CODETRACER_EVM_RECORDER_OUT_DIR";

/// Environment variable that, when set to `1`/`true`, disables tracing
/// entirely — the recorder runs as a pass-through (no Anvil spin-up, no
/// output written).  Convention: §5.
const ENV_DISABLED: &str = "CODETRACER_EVM_RECORDER_DISABLED";

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// CodeTracer EVM recorder — record Solidity/EVM execution traces.
///
/// Traces are always written in the canonical CTFS multi-stream format.
/// To convert a recorded `.ct` bundle to JSON / text for inspection, use
/// `ct print` from `codetracer-trace-format-nim`.
#[derive(Debug, Parser)]
#[command(
    name = "codetracer-evm-recorder",
    version,
    about = "Record EVM smart-contract execution traces for CodeTracer (CTFS-only). \
             Use `ct print` from codetracer-trace-format-nim for human-readable conversion.",
    long_about = "Record EVM smart-contract execution traces for CodeTracer.\n\
                  \n\
                  Output is always written in the canonical CodeTracer CTFS\n\
                  multi-stream format. Use `ct print` (shipped with the\n\
                  codetracer-trace-format-nim sibling) to convert a recorded\n\
                  `.ct` bundle to JSON or other human-readable forms.\n\
                  \n\
                  Environment variables:\n\
                    CODETRACER_EVM_RECORDER_OUT_DIR    fallback for --out-dir\n\
                    CODETRACER_EVM_RECORDER_DISABLED   set to 1/true to skip recording"
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
    /// CodeTracer CTFS bundle to `--out-dir`.
    Record(RecordArgs),
}

#[derive(Debug, clap::Args)]
struct RecordArgs {
    /// Path to the Solidity source file (.sol).
    solidity_file: PathBuf,

    /// Directory where the trace files will be written.
    ///
    /// The directory will be created if it does not exist.  A canonical
    /// CTFS `.ct` bundle is written here, alongside a copy of the source
    /// file so the db-backend can resolve source paths when the trace is
    /// loaded.
    ///
    /// Falls back to the `CODETRACER_EVM_RECORDER_OUT_DIR` environment
    /// variable when the flag is omitted.
    #[arg(short = 'o', long)]
    out_dir: Option<PathBuf>,

    /// Deprecated alias for `--out-dir`.  Still accepted so existing
    /// scripts keep working; emits a one-line stderr deprecation note
    /// when used.  Will be removed in a future release.
    #[arg(long, hide = true, value_name = "PATH")]
    trace_dir: Option<PathBuf>,

    /// Name of the function to call (without argument types or parentheses).
    ///
    /// Defaults to `run`. If a function named `run` does not exist in the
    /// ABI, the first non-constructor function is used instead.
    #[arg(long, default_value = "run")]
    function: String,

    /// Optional caller address (`0x...` hex) used as the `from` account
    /// for the function-call transaction.
    ///
    /// Anvil pre-funds 10 deterministic accounts and signs for any of
    /// them when a transaction's `from` field is set; passing a non-zero
    /// account here lets test fixtures exercise access-control failure
    /// paths (e.g. `onlyOwner` modifiers) where the deployer
    /// (`accounts[0]`) is the contract owner and a different signer must
    /// trigger the revert.
    ///
    /// Aliases: `--caller` (kept for symmetry with revert-handling
    /// nomenclature).  Both flags accept the same canonical EIP-55 hex
    /// representation.
    ///
    /// When omitted the deploy account (`accounts[0]`) is used as the
    /// caller, preserving historical behaviour.
    #[arg(long, alias = "caller", value_name = "ADDRESS")]
    from: Option<String>,

    /// Optional `value` (wei) attached to the function-call transaction.
    ///
    /// Required for exercising `payable` dispatcher checks.  When the
    /// value is non-zero AND the target function is `nonpayable`, the
    /// solc-generated dispatcher inserts a `CALLVALUE != 0 → revert`
    /// guard BEFORE any user code runs; the recorder must surface
    /// that revert as a distinct `EventLogKind::Error` io_event.
    ///
    /// Defaults to `0` (no ETH attached), preserving historical
    /// behaviour.  Accepts decimal (e.g. `100`) or `0x`-prefixed hex.
    #[arg(long, value_name = "WEI", default_value = "0")]
    value: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the effective output directory:
///   1. `--out-dir` if given on the CLI.
///   2. `--trace-dir` (deprecated alias) if given on the CLI; emits a
///      one-line stderr deprecation note.
///   3. `CODETRACER_EVM_RECORDER_OUT_DIR` env var.
///   4. Returns an error if none of the above is set (this recorder has
///      no usable default since trace dirs typically need to live next
///      to other CodeTracer artefacts; the convention default
///      `./ct-traces/` would also be acceptable but the EVM recorder
///      historically required an explicit path so we keep that contract).
fn resolve_out_dir(
    cli_out_dir: Option<PathBuf>,
    cli_trace_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = cli_out_dir {
        return Ok(path);
    }
    if let Some(path) = cli_trace_dir {
        eprintln!("warning: --trace-dir is deprecated, use --out-dir");
        return Ok(path);
    }
    if let Some(value) = std::env::var_os(ENV_OUT_DIR)
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    Err(eyre::eyre!(
        "no output directory specified: pass --out-dir <PATH> (or set {ENV_OUT_DIR})"
    ))
}

/// Whether the recorder is disabled via env var.  When true, the CLI
/// must execute in pass-through mode without emitting any trace
/// artefacts.  The EVM recorder doesn't run a separate target subprocess
/// (it spins up Anvil, deploys, and calls the contract itself), so
/// "disabled" simply means "don't emit any trace artefacts and skip the
/// Anvil round-trip".
fn recording_disabled() -> bool {
    match std::env::var(ENV_DISABLED) {
        Ok(value) => {
            let v = value.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
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

    if recording_disabled() {
        eprintln!(
            "{ENV_DISABLED} is set; skipping trace recording (no output written, Anvil not spawned)."
        );
        return Ok(());
    }

    let out_dir_path = resolve_out_dir(args.out_dir.clone(), args.trace_dir.clone())?;
    let out_dir = &out_dir_path;
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // Pure-Yul (.yul) files take a different compile path: solc's
    // `--strict-assembly` mode rejects the `--combined-json` invocation
    // used for Solidity files, so we shell out to a dedicated
    // assembly-mode invocation, synthesize a runtime source map from
    // the resulting `--asm-json` document, and run the recorder
    // pipeline with no ABI dispatch (calls go through with empty
    // calldata) and no Solidity AST or storage layout.  See
    // `yul_compile.rs` for the asm-json -> SourceMap conversion.
    let is_yul = source_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("yul"))
        .unwrap_or(false);
    if is_yul {
        return record_yul(args, &source_path, out_dir).await;
    }

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

    // Build the canonical function signature (`name(t1,t2,...)`) from
    // the ABI entry so we can compute the correct selector AND
    // auto-encode default arguments for simple primitive parameter
    // lists.  This lets the CLI invoke parameterised functions like
    // `setValue(uint256)` directly from a fixture, which is required
    // to drive access-control failure paths (e.g. the `onlyOwner`
    // modifier test) where the wrapped function takes a parameter.
    let (call_signature, encoded_args) = build_call_signature_and_args(&abi, &function_name)?;
    eprintln!(
        "Calling function: {} on contract {}",
        call_signature, contract_name
    );

    let selector = &alloy::primitives::keccak256(call_signature.as_bytes())[..4];
    let mut call_input = Vec::with_capacity(4 + encoded_args.len());
    call_input.extend_from_slice(selector);
    call_input.extend_from_slice(&encoded_args);

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
    let deploy_from = accounts[0];

    // Resolve the caller address for the function-call transaction.
    //
    // The deploy transaction always runs from `accounts[0]` (so the
    // contract's `owner = msg.sender` constructors pin the owner to a
    // stable, well-known address).  The function-call transaction's
    // caller is whatever `--from` was set to, defaulting to the deploy
    // account.  Passing a different address from anvil's pre-funded
    // accounts lets test fixtures hit access-control failure paths
    // (e.g. the `onlyOwner` modifier in `Modifier.sol`).
    let call_from: Address = match args.from.as_deref() {
        Some(s) => Address::from_str(s)
            .with_context(|| format!("--from is not a valid 0x-prefixed hex address: {s}"))?,
        None => deploy_from,
    };

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
        .from(deploy_from)
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
    //
    // We deliberately set an explicit `gas_limit` so anvil mines the
    // transaction even when it reverts.  Without `gas` set, alloy's
    // `send_transaction` runs `eth_estimateGas` first; on a reverting
    // call (`require(false, "...")`, `revert("...")`, panics)
    // estimation fails and the JSON-RPC call returns an error before
    // the transaction is ever included in a block — so no
    // `debug_traceTransaction` data is available, and the recorder
    // cannot capture the structlog up to the REVERT opcode.
    //
    // By pinning `gas_limit` to the block gas limit we let anvil
    // include the transaction; the receipt comes back with
    // `status == false`, but the structlog is fully populated and we
    // can surface the revert reason as an `EventLogKind::Error`
    // io_event (see step 7 / 9 below).  This matches the cross-recorder
    // expectation captured in the
    // `test_require_revert_failing_path_emits_error_event` test.
    // -----------------------------------------------------------------------
    let call_value = parse_value_wei(&args.value)
        .with_context(|| format!("--value is not a valid wei amount: {}", args.value))?;
    let call_tx = alloy::rpc::types::TransactionRequest::default()
        .from(call_from)
        .to(contract_address)
        .with_input(alloy::primitives::Bytes::from(call_input))
        .value(call_value)
        .gas_limit(30_000_000);

    let call_pending = provider
        .send_transaction(call_tx)
        .await
        .context("failed to send function call transaction")?;
    let call_receipt = call_pending
        .get_receipt()
        .await
        .context("failed to get function call receipt")?;
    let tx_hash = call_receipt.transaction_hash;
    let tx_succeeded = call_receipt.status();

    eprintln!(
        "Transaction: {:?} (status={})",
        tx_hash,
        if tx_succeeded { "ok" } else { "reverted" }
    );

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
    // out_dir we ensure the path remains valid even when the caller moves
    // the trace around.
    // -----------------------------------------------------------------------
    let source_filename = source_path
        .file_name()
        .ok_or_else(|| eyre::eyre!("source path has no filename component"))?;
    let source_copy_path = out_dir.join(source_filename);
    std::fs::copy(&source_path, &source_copy_path).with_context(|| {
        format!(
            "failed to copy source file into trace dir: {} -> {}",
            source_path.display(),
            source_copy_path.display()
        )
    })?;

    // -----------------------------------------------------------------------
    // 9. Process through the recorder and write trace output
    //
    // `metadata.program` carries the source-file path the user passed in
    // (per cross-recorder convention captured in
    // `metacraft-specs/policies/recorder-test-requirements.md` §1), NOT
    // the contract name.  Pre-2026-05 the EVM recorder labelled the
    // trace with `contract_name` — that was an EVM-only deviation from
    // the path-as-program convention used by the PHP / Ruby / Python /
    // Cardano / TON recorders, and surfaced as the
    // `test_control_flow_metadata_program_is_source_path` bug.
    // -----------------------------------------------------------------------
    let program_label = source_path.to_string_lossy();
    let mut recorder =
        EvmRecorder::new(&program_label, out_dir).context("failed to create EvmRecorder")?;
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

    // -----------------------------------------------------------------------
    // 9b. Surface revert reason for failed transactions
    //
    // For a reverting tx (`require(false, "msg")`, `revert("msg")`,
    // `Panic(uint256)`, custom errors, ...) `frame.failed` is true and
    // `frame.return_value` carries the ABI-encoded revert payload.  We
    // decode the payload and emit an `EventLogKind::Error` io_event so
    // consumers see the reason alongside the partial structlog (the
    // structlog is already faithfully captured up to the REVERT
    // opcode by step 9 above).
    //
    // Without this step the .ct bundle would still be produced but the
    // user would have no way to tell *why* the transaction reverted —
    // they'd only see execution stop mid-function.  The
    // `test_require_revert_failing_path_emits_error_event` test pins
    // this contract.
    // -----------------------------------------------------------------------
    if frame.failed || !tx_succeeded {
        // Build a custom-error registry from the contract ABI so
        // typed `revert Foo(arg1, arg2)` payloads surface as
        // `Foo(name1=val1, name2=val2)` instead of opaque hex.
        let registry = revert_decode::CustomErrorRegistry::from_abi(&abi);
        let decoded = revert_decode::decode_revert_with_registry(&frame.return_value, &registry);
        eprintln!(
            "Transaction reverted: {} ({})",
            decoded.message, decoded.kind
        );
        recorder.register_revert(decoded.kind, &decoded.message);
    }

    recorder
        .finalize()
        .context("failed to finalize EvmRecorder")?;

    eprintln!("Trace written to {}", out_dir.display());
    eprintln!("  {}", source_filename.to_string_lossy());

    Ok(())
}

/// Pure-Yul recorder path.
///
/// Mirrors [`record`] but with a dedicated compile + dispatch
/// pipeline:
///
///   1. Compile via `solc --strict-assembly --bin --asm-json` (see
///      `yul_compile.rs`).
///   2. Spin up a transient anvil node with `--steps-tracing`.
///   3. Deploy the constructor bytecode (the outer `object` that
///      copies the runtime to memory and RETURNs it).
///   4. Fetch the deployed runtime bytecode via `eth_getCode` -- this
///      is what executes for every call regardless of calldata, and
///      what the synthesized source map is indexed against.
///   5. Send a single transaction with empty calldata (Yul has no
///      ABI dispatcher and no selector).
///   6. Run the EVM recorder with the synthesized source map and
///      `None` for the Solidity AST / storage layout.
///
/// The strict pin in
/// `tests/test_programs_via_ct_print_full.rs::test_yul_pure_via_ct_print_full`
/// asserts that Yul function calls (`function foo(...) -> r`) surface
/// as nested call frames via the source map's [in]/[out] jump-type
/// markers, and that the on-chain return value matches the
/// arithmetic the Yul program performs.
async fn record_yul(args: RecordArgs, source_path: &Path, out_dir: &Path) -> Result<()> {
    // -----------------------------------------------------------------------
    // 1. Compile the Yul source file with solc --strict-assembly
    // -----------------------------------------------------------------------
    let solc_cmd = std::env::var("SOLC_PATH").unwrap_or_else(|_| "solc".to_string());
    let yul_out = yul_compile::compile_yul(&solc_cmd, source_path)?;

    let source_contents = std::fs::read_to_string(source_path)
        .with_context(|| format!("failed to read source file: {}", source_path.display()))?;

    // The Yul object name is the file stem (matches the
    // `object "Name" { ... }` declaration by convention).  Used as
    // the contract label in stderr breadcrumbs.
    let object_name = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("YulObject");

    // -----------------------------------------------------------------------
    // 2. Spin up a local Anvil node
    // -----------------------------------------------------------------------
    let anvil = alloy::node_bindings::Anvil::new()
        .arg("--steps-tracing")
        .spawn();
    let rpc_url = anvil.endpoint();

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let accounts = provider.get_accounts().await?;
    let deploy_from = accounts[0];

    let call_from: Address = match args.from.as_deref() {
        Some(s) => Address::from_str(s)
            .with_context(|| format!("--from is not a valid 0x-prefixed hex address: {s}"))?,
        None => deploy_from,
    };

    eprintln!("Calling Yul object {object_name} (no ABI dispatcher; empty calldata)");

    // -----------------------------------------------------------------------
    // 3. Deploy the contract
    // -----------------------------------------------------------------------
    let deploy_data = alloy::primitives::Bytes::from(yul_out.deploy_bytecode.clone());
    let deploy_tx = alloy::rpc::types::TransactionRequest::default()
        .from(deploy_from)
        .with_deploy_code(deploy_data);

    let deploy_pending = provider
        .send_transaction(deploy_tx)
        .await
        .context("failed to send Yul deploy transaction")?;
    let deploy_receipt = deploy_pending
        .get_receipt()
        .await
        .context("failed to get Yul deploy receipt")?;
    let contract_address = deploy_receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("Yul deploy transaction produced no contract address"))?;

    eprintln!("Deployed Yul object {object_name} at {contract_address}");

    // -----------------------------------------------------------------------
    // 4. Fetch the deployed runtime bytecode
    //
    // Pure-Yul objects don't expose `bin-runtime` from solc's
    // assembly mode (`solc --strict-assembly --bin-runtime` is
    // rejected with `not supported in assembler mode`), so we read
    // the deployed bytecode straight off the chain.  This is what
    // the synthesized source map is indexed against.
    // -----------------------------------------------------------------------
    let runtime_bytecode_bytes = provider.get_code_at(contract_address).await?;
    let runtime_bytecode: Vec<u8> = runtime_bytecode_bytes.to_vec();
    if runtime_bytecode.is_empty() {
        return Err(eyre::eyre!(
            "deployed Yul object has no runtime bytecode (eth_getCode returned 0 bytes)"
        ));
    }

    // -----------------------------------------------------------------------
    // 5. Call the contract with empty calldata
    // -----------------------------------------------------------------------
    let call_value = parse_value_wei(&args.value)
        .with_context(|| format!("--value is not a valid wei amount: {}", args.value))?;
    let call_tx = alloy::rpc::types::TransactionRequest::default()
        .from(call_from)
        .to(contract_address)
        .with_input(alloy::primitives::Bytes::new())
        .value(call_value)
        .gas_limit(30_000_000);

    let call_pending = provider
        .send_transaction(call_tx)
        .await
        .context("failed to send Yul function call transaction")?;
    let call_receipt = call_pending
        .get_receipt()
        .await
        .context("failed to get Yul function call receipt")?;
    let tx_hash = call_receipt.transaction_hash;
    let tx_succeeded = call_receipt.status();

    eprintln!(
        "Transaction: {:?} (status={})",
        tx_hash,
        if tx_succeeded { "ok" } else { "reverted" }
    );

    // -----------------------------------------------------------------------
    // 6. Fetch debug_traceTransaction structlogs
    // -----------------------------------------------------------------------
    let frame = trace_fetcher::fetch_struct_logs(&rpc_url, tx_hash)
        .await
        .context("failed to fetch structlogs via debug_traceTransaction")?;
    let struct_logs = trace_fetcher::extract_struct_logs(&frame);
    eprintln!("Fetched {} struct log entries", struct_logs.len());
    if struct_logs.is_empty() {
        return Err(eyre::eyre!(
            "debug_traceTransaction returned no structlogs -- ensure anvil was started with --steps-tracing"
        ));
    }

    // -----------------------------------------------------------------------
    // 7. Copy the source file into the trace directory
    // -----------------------------------------------------------------------
    let source_filename = source_path
        .file_name()
        .ok_or_else(|| eyre::eyre!("source path has no filename component"))?;
    let source_copy_path = out_dir.join(source_filename);
    std::fs::copy(source_path, &source_copy_path).with_context(|| {
        format!(
            "failed to copy source file into trace dir: {} -> {}",
            source_path.display(),
            source_copy_path.display()
        )
    })?;

    // -----------------------------------------------------------------------
    // 8. Process through the recorder and write trace output
    //
    // Pure-Yul has no Solidity AST and no storage-layout document, so
    // we pass `None` for both.  Internal-call resolution still works
    // because the source map's [in]/[out] jump-type markers drive the
    // recorder's call-frame tracking even without an AST.
    // -----------------------------------------------------------------------
    let program_label = source_path.to_string_lossy();
    let mut recorder =
        EvmRecorder::new(&program_label, out_dir).context("failed to create EvmRecorder")?;
    recorder
        .initialize()
        .context("failed to initialize EvmRecorder")?;

    let source_path_ref: &Path = source_copy_path.as_path();
    recorder
        .record_from_structlog(
            struct_logs,
            &yul_out.runtime_source_map,
            &runtime_bytecode,
            &[source_path_ref],
            &[source_contents.as_str()],
            None,
            None,
        )
        .context("recorder failed to process structlogs")?;

    if frame.failed || !tx_succeeded {
        let registry = revert_decode::CustomErrorRegistry::default();
        let decoded = revert_decode::decode_revert_with_registry(&frame.return_value, &registry);
        eprintln!(
            "Transaction reverted: {} ({})",
            decoded.message, decoded.kind
        );
        recorder.register_revert(decoded.kind, &decoded.message);
    }

    recorder
        .finalize()
        .context("failed to finalize EvmRecorder")?;

    eprintln!("Trace written to {}", out_dir.display());
    eprintln!("  {}", source_filename.to_string_lossy());

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the canonical function call signature (e.g. `setValue(uint256)`)
/// and the ABI-encoded calldata args for the function named `function_name`.
///
/// Falls back to `name()` (no args) when the ABI entry is missing — that
/// preserves the old behaviour for callers that don't depend on ABI
/// lookup.  For parameterised functions we synthesise sensible defaults
/// for the few primitive shapes the CLI currently encodes
/// (see [`encode_default_arg_value`]); anything else returns an error
/// asking the caller to provide a no-argument entry-point function.
///
/// This generality matters because some test fixtures need to hit the
/// failing branch of a guarded function (e.g. an `onlyOwner` modifier
/// guarding `setValue(uint256)`) rather than wrapping it in a
/// parameterless wrapper — the failing-modifier test
/// (`test_modifier_failing_path_emits_error_event`) relies on this.
fn build_call_signature_and_args(
    abi: &[serde_json::Value],
    function_name: &str,
) -> Result<(String, Vec<u8>)> {
    // Find the matching ABI entry, if any.
    let entry = abi.iter().find(|item| {
        item.get("type").and_then(|v| v.as_str()) == Some("function")
            && item.get("name").and_then(|v| v.as_str()) == Some(function_name)
    });

    let inputs: &[serde_json::Value] = entry
        .and_then(|e| e.get("inputs"))
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    // Build the canonical signature: name(t1,t2,...).
    let type_list: Vec<&str> = inputs
        .iter()
        .map(|p| p.get("type").and_then(|v| v.as_str()).unwrap_or(""))
        .collect();
    let signature = format!("{}({})", function_name, type_list.join(","));

    // Encode each argument with a sensible default.
    let mut encoded = Vec::with_capacity(inputs.len() * 32);
    for (idx, ty) in type_list.iter().enumerate() {
        let bytes = encode_default_arg_value(ty).ok_or_else(|| {
            eyre::eyre!(
                "function `{}` parameter #{} has type `{}` for which the CLI \
                 cannot synthesise a default value; pass a parameterless \
                 wrapper as the entry-point",
                function_name,
                idx,
                ty
            )
        })?;
        encoded.extend_from_slice(&bytes);
    }
    Ok((signature, encoded))
}

/// Parse a `--value` argument into a `U256` wei amount.
///
/// Accepts decimal (`100`) or `0x`-prefixed hex (`0x64`).  Empty
/// strings are treated as zero so the CLI default `--value 0`
/// preserves the historical no-value-attached behaviour.
fn parse_value_wei(s: &str) -> Result<alloy::primitives::U256> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(alloy::primitives::U256::ZERO);
    }
    if let Some(rest) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        Ok(alloy::primitives::U256::from_str_radix(rest, 16)
            .with_context(|| format!("invalid hex value: {s}"))?)
    } else {
        Ok(alloy::primitives::U256::from_str_radix(trimmed, 10)
            .with_context(|| format!("invalid decimal value: {s}"))?)
    }
}

/// Encode a default value for the given Solidity ABI type.  Currently
/// supports `uint*` / `int*` (defaults to `7` — non-zero and small,
/// matching the canonical fixture pattern), `bool` (false) and
/// `address` (zero address).  Returns `None` for any other type so
/// the caller surfaces a precise diagnostic.
fn encode_default_arg_value(ty: &str) -> Option<[u8; 32]> {
    let mut buf = [0u8; 32];
    if ty.starts_with("uint") || ty.starts_with("int") {
        // Default to 7 — small, non-zero, matches the modifier_test
        // fixture's expected `setValue(7)` invocation.
        buf[31] = 7;
        Some(buf)
    } else if ty == "bool" || ty == "address" || ty == "address payable" {
        // bool: false; address[ payable]: zero-padded.
        Some(buf)
    } else if let Some(rest) = ty.strip_prefix("bytes") {
        // bytes1..bytes32: zero-padded fixed-size bytes default.
        if rest
            .parse::<u32>()
            .ok()
            .is_some_and(|n| (1..=32).contains(&n))
        {
            return Some(buf);
        }
        None
    } else {
        None
    }
}

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
