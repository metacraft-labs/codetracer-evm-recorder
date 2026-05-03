//! CTFS audit smoke test for the EVM recorder.
//!
//! Verifies that the recorder emits the canonical CTFS event set required by
//! the codetracer frontend / db-backend.  This complements the existing e2e
//! tests, which focus on bytecode execution and source-mapping correctness,
//! by walking the produced `.ct` container through the public Nim
//! `NimTraceReaderHandle` API and asserting on the deserialised data.
//!
//! See `AUDIT-CTFS-2026-05.md` and the IsoNim-migration handoff entry 1.39
//! for the audit checklist this test pins down.
//!
//! Requires `solc` and `anvil` on PATH (provided by the Nix dev shell).

use std::path::Path;
use std::process::Command;

use alloy::network::TransactionBuilder;
use alloy::primitives::Bytes;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;

use codetracer_evm_recorder::recorder::EvmRecorder;
use codetracer_evm_recorder::solidity_ast::SolidityAst;
use codetracer_evm_recorder::source_map::SourceMap;
use codetracer_evm_recorder::storage_layout::StorageLayout;
use codetracer_evm_recorder::trace_fetcher;

use codetracer_trace_writer_nim::NimTraceReaderHandle;

// ---------------------------------------------------------------------------
// Toolchain detection
// ---------------------------------------------------------------------------

fn has_solc() -> bool {
    Command::new("solc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn has_anvil() -> bool {
    Command::new("anvil")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Recording pipeline
// ---------------------------------------------------------------------------

/// Compile `contracts/FlowTest.sol`, deploy it on a local anvil node, call
/// the canonical `compute()` entrypoint, fetch the structlogs, and run the
/// EVM recorder against them.  Returns the temporary trace directory holding
/// the produced `.ct` container.
async fn record_flow_test() -> tempfile::TempDir {
    // ---- compile ---------------------------------------------------------
    let solc_out = Command::new("solc")
        .args([
            "--combined-json",
            "abi,bin,bin-runtime,srcmap-runtime,storage-layout,ast",
            "--no-cbor-metadata",
            "contracts/FlowTest.sol",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run solc");
    assert!(
        solc_out.status.success(),
        "solc failed: {}",
        String::from_utf8_lossy(&solc_out.stderr)
    );
    let raw_json = String::from_utf8(solc_out.stdout).unwrap();
    let compiled: serde_json::Value = serde_json::from_str(&raw_json).unwrap();
    let ast = SolidityAst::from_combined_json(&raw_json).unwrap();

    let contracts = compiled["contracts"].as_object().unwrap();
    let (_, contract) = contracts
        .iter()
        .find(|(k, _)| k.ends_with(":FlowTest"))
        .expect("FlowTest contract missing");

    let deploy_hex = contract["bin"].as_str().unwrap().to_string();
    let runtime_hex = contract["bin-runtime"].as_str().unwrap();
    let srcmap_raw = contract["srcmap-runtime"].as_str().unwrap();
    let source_map = SourceMap::parse(srcmap_raw);
    let storage_layout: Option<StorageLayout> =
        serde_json::from_value(contract["storage-layout"].clone()).ok();
    let runtime_bytecode = alloy::hex::decode(runtime_hex).expect("invalid runtime bytecode");

    let source_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/FlowTest.sol");
    let source_contents = std::fs::read_to_string(&source_path).unwrap();

    // ---- spawn anvil + deploy + call ------------------------------------
    let anvil = alloy::node_bindings::Anvil::new()
        .arg("--steps-tracing")
        .spawn();
    let rpc_url = anvil.endpoint();
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse().unwrap());
    let from = provider.get_accounts().await.unwrap()[0];

    let deploy_tx = TransactionRequest::default()
        .from(from)
        .with_deploy_code(Bytes::from(alloy::hex::decode(&deploy_hex).unwrap()));
    let deploy_receipt = provider
        .send_transaction(deploy_tx)
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();
    let contract_address = deploy_receipt.contract_address.unwrap();

    let selector = &alloy::primitives::keccak256("compute()".as_bytes())[..4];
    let call_tx = TransactionRequest::default()
        .from(from)
        .to(contract_address)
        .with_input(Bytes::copy_from_slice(selector));
    let receipt = provider
        .send_transaction(call_tx)
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();

    let frame = trace_fetcher::fetch_struct_logs(&rpc_url, receipt.transaction_hash)
        .await
        .unwrap();
    let struct_logs = trace_fetcher::extract_struct_logs(&frame);
    assert!(!struct_logs.is_empty(), "structLogs unexpectedly empty");

    // ---- record ---------------------------------------------------------
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let mut recorder = EvmRecorder::new("FlowTest", tmp_dir.path()).unwrap();
    recorder.initialize().unwrap();
    recorder
        .record_from_structlog(
            struct_logs,
            &source_map,
            &runtime_bytecode,
            &[source_path.as_path()],
            &[source_contents.as_str()],
            storage_layout.as_ref(),
            Some(&ast),
        )
        .unwrap();
    recorder.finalize().unwrap();

    tmp_dir
}

/// Locate the single `.ct` file produced by the recorder in `dir`.
fn find_ct_container(dir: &Path) -> std::path::PathBuf {
    std::fs::read_dir(dir)
        .expect("trace dir unreadable")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "ct"))
        .expect("no .ct container in trace dir")
}

/// Open the recorded .ct via the Nim trace-reader FFI.
fn open_reader(dir: &Path) -> NimTraceReaderHandle {
    let ct_path = find_ct_container(dir);
    NimTraceReaderHandle::open(ct_path.to_str().unwrap())
        .unwrap_or_else(|e| panic!("failed to open .ct via Nim FFI: {}", e))
}

// ---------------------------------------------------------------------------
// CTFS audit assertions
// ---------------------------------------------------------------------------

/// Audit (a): Call records are emitted for internal Solidity calls and the
/// target function's source-level name (resolved via the Solidity AST) is
/// preserved in the FunctionRecord.  FlowTest::compute() invokes the
/// internal `add()` helper so we expect to see it referenced from at least
/// one CallRecord in the trace.
#[tokio::test]
async fn audit_ctfs_internal_call_emitted() {
    if !has_solc() || !has_anvil() {
        eprintln!("skipping: solc/anvil unavailable");
        return;
    }

    let tmp_dir = record_flow_test().await;
    let reader = open_reader(tmp_dir.path());

    let call_count = reader.call_count();
    assert!(
        call_count > 0,
        "no Call records in trace — recorder emitted zero internal calls"
    );

    // Build the function-id → name table.  The Nim reader's
    // `function(id)` returns the function NAME directly (interned string)
    // for 0-indexed function ids — see
    // `codetracer_trace_writer_nim::NimTraceReaderHandle::function`.
    let fn_count = reader.function_count();
    let mut fn_names: Vec<String> = Vec::with_capacity(fn_count as usize);
    for i in 0..fn_count {
        fn_names.push(reader.function(i).expect("function name missing"));
    }

    let mut targets: Vec<String> = Vec::with_capacity(call_count as usize);
    for k in 0..call_count {
        let raw = reader.call_json(k).expect("call record JSON missing");
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // The Nim multi-stream call stream stores function ids 1-indexed
        // (function_id=0 is reserved for the implicit toplevel /
        // unset state).  See `multi_stream_writer.nim::registerCall`.
        // Subtract 1 to map back to the 0-indexed `function()` table.
        let fid_raw = parsed["function_id"]
            .as_u64()
            .or_else(|| parsed["functionId"].as_u64())
            .unwrap_or(u64::MAX);
        let fid = fid_raw.saturating_sub(1);
        let name = fn_names
            .get(fid as usize)
            .cloned()
            .unwrap_or_else(|| format!("<unknown:{}>", fid_raw));
        targets.push(name);
    }

    // Solidity AST resolution should have replaced the legacy
    // `fn_at_<file>:<line>` placeholder for the internal `add` call.  This
    // pins the AST-aware function-name fix.
    assert!(
        targets.iter().any(|n| n == "add"),
        "expected an internal Call to `add`, got: {:?}",
        targets
    );
}

/// Audit (c): EVM LOG opcodes route through register_special_event with the
/// canonical `EventLogKind::EvmEvent` kind (numeric 13) — not Write (0) or
/// WriteOther (2) which are reserved for stdout/stderr-style I/O streams.
///
/// This is the EVM analogue of the JS recorder's stderr→WriteOther fix from
/// handoff entry 1.38.  EVM has no stdout concept; misrouting LOG events as
/// `Write` makes them surface in the terminal-output pane alongside Python /
/// Ruby print output.  `EvmEvent` is the kind the codetracer frontend's
/// `event_log.nim` and `flow.nim` special-case for EVM-style structured
/// events (matches the Stylus tracer convention; see
/// `codetracer/src/db-backend/tests/stylus_flow_integration.rs`).
#[tokio::test]
async fn audit_ctfs_log_event_kind_is_evmevent() {
    if !has_solc() || !has_anvil() {
        eprintln!("skipping: solc/anvil unavailable");
        return;
    }

    let tmp_dir = record_flow_test().await;
    let reader = open_reader(tmp_dir.path());

    let event_count = reader.event_count();
    assert!(
        event_count > 0,
        "no Event records emitted — FlowTest::compute() does emit a `Computed(uint256)` log"
    );

    // The Nim multi-stream writer collapses the 13-variant `EventLogKind`
    // into 4 multi-stream `IOEventKind` buckets (stdout / stderr / fileOp /
    // error) — see `codetracer_trace_writer_ffi.nim::toIOEventKind`.
    // The relevant collapse for this audit:
    //
    //     EventLogKind::Write      (stdout-style writes)        → "stdout"
    //     EventLogKind::WriteOther (non-stdout writes)          → "stdout"
    //     EventLogKind::EvmEvent   (EVM LOG opcodes / Stylus)   → "stderr"
    //     EventLogKind::TraceLogEvent                            → "stderr"
    //     EventLogKind::Error                                    → "error"
    //
    // Pre-fix, the EVM recorder emitted LOG opcodes as `Write` →
    // `stdout`, which mixed them with stdout terminal writes from
    // recorders like Python/Ruby.  Post-fix (this audit), they emit as
    // `EvmEvent` → `stderr`, segregating them from stdout.  We assert
    // every Event record from the EVM recorder lands in the `stderr`
    // bucket and none in `stdout` — that's the canonical
    // "EvmEvent-as-the-CTFS-multi-stream-presents-it" check.
    //
    // This is necessarily weaker than the JS recorder's audit-time
    // check (1.38) which compares the raw `RecordEvent.kind` byte
    // against the upstream enum, because the multi-stream IO format
    // has discarded the original kind by the time we read it back.
    // See `AUDIT-CTFS-2026-05.md` for the open infrastructure
    // follow-up that would preserve `EvmEvent` end-to-end.
    let mut stderr_count = 0usize;
    let mut other_kinds = Vec::new();
    for i in 0..event_count {
        let raw = reader.event_json(i).expect("event record JSON missing");
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let kind = parsed["kind"].as_str().unwrap_or("<missing>").to_string();
        if kind == "stderr" {
            stderr_count += 1;
        } else {
            other_kinds.push(kind);
        }
    }
    assert!(
        other_kinds.is_empty(),
        "EVM recorder produced events with non-EvmEvent kinds: {:?}",
        other_kinds
    );
    assert!(stderr_count > 0, "no stderr-bucket events found");
}

/// Audit (e): Step records are emitted for source-line navigation.
///
/// FlowTest::compute() spans multiple Solidity source lines (variable
/// initialisations, the internal call, the SSTOREs, the emit, and the
/// return).  The recorder must emit at least one Step record per visited
/// source line so the frontend's "next line" navigation can land on them.
#[tokio::test]
async fn audit_ctfs_step_records_emitted() {
    if !has_solc() || !has_anvil() {
        eprintln!("skipping: solc/anvil unavailable");
        return;
    }

    let tmp_dir = record_flow_test().await;
    let reader = open_reader(tmp_dir.path());

    let step_count = reader.step_count();
    // FlowTest::compute() has 7 source statements + add() body.
    assert!(
        step_count >= 5,
        "expected at least 5 Step records (compute() body + add()), got {}",
        step_count
    );
}

/// Audit (b) diagnostic: the recorder can now recover and stage Solidity
/// internal-call parameters, but the current Nim writer dependency still
/// reads the resulting CTFS `Call.args` back as empty.  Keep this guard until
/// the writer-side `register_call_arg` attachment path is fixed, then replace
/// it with a positive `add(x, y)` readback assertion.
#[tokio::test]
async fn audit_ctfs_call_args_writer_gap_known_empty() {
    if !has_solc() || !has_anvil() {
        eprintln!("skipping: solc/anvil unavailable");
        return;
    }

    let tmp_dir = record_flow_test().await;
    let reader = open_reader(tmp_dir.path());

    let fn_count = reader.function_count();
    let mut fn_names: Vec<String> = Vec::with_capacity(fn_count as usize);
    for i in 0..fn_count {
        fn_names.push(reader.function(i).expect("function name missing"));
    }

    // Narrow the remaining gap: `TraceWriter::arg` registers the EVM
    // parameters as varnames / step values, so source-level recovery and the
    // variable side of the Nim writer FFI are both live.  The failure is the
    // separate pending-call-arg attachment consumed by `register_call`.
    let mut varnames = Vec::new();
    for i in 0..reader.varname_count() {
        varnames.push(reader.varname(i).expect("varname missing"));
    }
    assert!(
        varnames.iter().any(|name| name == "x") && varnames.iter().any(|name| name == "y"),
        "expected staged add(x, y) names in the CTFS varname table, got {:?}",
        varnames
    );

    let mut add_call_key = None;
    let mut call_summaries = Vec::new();
    let mut calls_with_args = Vec::new();
    for k in 0..reader.call_count() {
        let raw = reader.call_json(k).expect("call record JSON missing");
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let fid_raw = parsed["function_id"]
            .as_u64()
            .or_else(|| parsed["functionId"].as_u64())
            .unwrap_or(u64::MAX);
        let fid = fid_raw.saturating_sub(1);
        let function_name = fn_names
            .get(fid as usize)
            .cloned()
            .unwrap_or_else(|| format!("<unknown:{fid_raw}>"));
        let args_len = parsed["args"].as_array().map_or(0, Vec::len);
        call_summaries.push(format!("{k}:{function_name}:args={args_len}"));
        if args_len > 0 {
            calls_with_args.push(format!("{k}:{function_name}:args={args_len}"));
        }
        if function_name == "add" {
            add_call_key = Some(k);
        }
    }

    let add_call_key = add_call_key.unwrap_or_else(|| {
        panic!(
            "expected a Call record for internal `add`; call summaries: {:?}",
            call_summaries
        )
    });
    assert_eq!(
        add_call_key, 0,
        "expected the first completed call record to be `add`; this rules \
         out an earlier call consuming the staged add(x, y) args. Call \
         summaries: {:?}",
        call_summaries
    );
    assert!(
        calls_with_args.is_empty(),
        "staged add(x, y) args were attached to a different call record: {:?}",
        calls_with_args
    );
    let raw = reader
        .call_json(add_call_key)
        .expect("add call record JSON missing");
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let args = parsed["args"]
        .as_array()
        .expect("add call args should be a JSON array");
    let arg_count = args.len();
    assert_eq!(
        arg_count, 0,
        "CTFS Call.args are now attached for add(x, y); replace this \
         diagnostic with a positive readback assertion and close the \
         writer-side follow-up in AUDIT-CTFS-2026-05.md. Call summaries: {:?}",
        call_summaries
    );
}
