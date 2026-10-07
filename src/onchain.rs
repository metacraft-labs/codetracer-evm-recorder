//! The on-chain recording route: a real transaction on a live EVM network
//! to a CodeTracer CTFS container.
//!
//! # Why this module exists
//!
//! Both halves of this route were already in the repository and nothing
//! joined them:
//!
//! * [`crate::replay::replay_transaction`] pins a fork at the target block's
//!   PARENT, replays the preceding transactions in order to rebuild the
//!   prestate, then replays the target transaction under
//!   [`crate::inspector::CodeTracerInspector`].  That is the
//!   `replay-preceding` prestate strategy: it asks the endpoint for archive
//!   reads and for no tracer at all.
//! * [`crate::recorder::EvmRecorder`] drives the Nim CTFS `TraceWriter`, but
//!   its input is a slice of `debug_traceTransaction` structLogs.
//!
//! The inspector's [`ExecutionData`] and the recorder's [`StructLog`] are
//! two shapes for the same thing, so the join is
//! [`execution_data_to_struct_logs`] plus the orchestration below.  Nothing
//! here needs `debug_traceTransaction`, which is what makes the route
//! usable: the public endpoints that serve mainnet archive state at depth
//! refuse the struct-log tracer, in two distinct refusal classes
//! (`-32601` absent, and `-32602`/403 policy).
//!
//! # Source resolution is best-effort, and says so
//!
//! A mainnet address has no local `.sol` file.  Verified source is
//! recovered from Sourcify and RECOMPILED, because the `srcmap-runtime`
//! that maps a PC back to a source span is a compiler output, not something
//! Sourcify serves.  Two things can go wrong and both are reported rather
//! than papered over:
//!
//! 1. The recompile needs the solc release the contract was verified with.
//!    [`resolve_solc_for_version`] looks for it; if only a different solc is
//!    available the recompile fails and the address stays unmapped.
//! 2. A recompile that SUCCEEDS can still produce a source map that does not
//!    describe the deployed code (wrong optimizer settings, different solc
//!    patch level, libraries).  Wrong line attributions are worse than
//!    none, so the recompiled runtime bytecode is checked against
//!    `eth_getCode` at the target block and mismatching artifacts are
//!    dropped unless the caller passes `allow_source_mismatch`.
//!
//! # Why an unverified address still produces a recording
//!
//! The recorder emits a step only where a source map resolves the PC to a
//! source location, so an address with no artifacts contributes NO steps.
//! Measured: the first mainnet container, written for an unverified
//! contract, held exactly the ONE step `TraceWriter::start` emits against a
//! replay that captured 556 — a container a reader opens and that is not a
//! recording of anything.
//!
//! So an address without usable verified source gets a DISASSEMBLY listing
//! of its deployed bytecode (see [`disassemble`]) plus a source map
//! pointing each instruction at its own line of that listing.  That is a
//! source file built from the bytecode itself, and it makes "recorded at
//! EVM-opcode granularity" true rather than aspirational: every opcode the
//! replay stepped through has somewhere to point.  `disassembly_fallback`
//! turns it off, for a caller who wants verified source or nothing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use alloy::primitives::{Address, TxHash};
use alloy::providers::Provider;
use eyre::{Context, Result};

use crate::contract_registry::{ContractArtifacts, ContractRegistry};
use crate::inspector::ExecutionData;
use crate::recorder::EvmRecorder;
use crate::replay::{
    ReplayOptions, ReplayedTransaction, build_provider, replay_transaction_detailed,
};
use crate::revert_decode;
use crate::source_fetcher;
use crate::source_map::{SourceMap, build_pc_to_instruction_index};
use crate::structlog::StructLog;
use codetracer_trace_writer_nim::NimTraceReaderHandle;

/// Hard cap on how many distinct addresses the route will ask Sourcify
/// about.  A single mainnet transaction can touch dozens of addresses (a
/// router hop through several pools); each one is an HTTP round trip plus a
/// solc invocation, and the ones that matter for source attribution are the
/// entry point and the handful of frames beneath it.
pub const DEFAULT_MAX_SOURCE_LOOKUPS: usize = 8;

// ---------------------------------------------------------------------------
// ExecutionData -> structLogs
// ---------------------------------------------------------------------------

/// Convert the inspector's captured steps into the structLog shape
/// [`EvmRecorder::record_from_structlog_multi_contract`] consumes.
///
/// Field-by-field, and what each one costs:
///
/// | structLog field | source |
/// |---|---|
/// | `pc`, `op`, `depth`, `stack` | captured directly by the inspector |
/// | `memory` | captured by the inspector when built with `with_memory_capture`, re-encoded as 32-byte hex words (geth's `enableMemory` shape) |
/// | `memory_size` | captured directly |
/// | `gas` | gas remaining before the opcode, as geth samples it |
/// | `gas_cost` | DERIVED: `gas[i] - gas[i+1]` while the depth is unchanged, else 0 |
/// | `storage`, `return_data`, `refund_counter`, `error` | `None` |
///
/// The four `None` fields are not read by the recorder — its SLOAD / SSTORE
/// decoding works off the stack, and revert reasons come from the
/// transaction's own return data, not from a per-step field.  They are left
/// unset rather than zero-filled so a future reader cannot mistake a
/// placeholder for a measurement.
///
/// `gas_cost` is derived rather than captured because the inspector's
/// `step` hook runs before the opcode is priced; across a frame boundary
/// the difference is not a cost at all, so it is reported as 0 there.
pub fn execution_data_to_struct_logs(data: &ExecutionData) -> Vec<StructLog> {
    let mut out = Vec::with_capacity(data.steps.len());
    for (i, step) in data.steps.iter().enumerate() {
        let gas_cost = match data.steps.get(i + 1) {
            Some(next) if next.depth == step.depth => step.gas.saturating_sub(next.gas),
            _ => 0,
        };
        out.push(StructLog {
            pc: step.pc as u64,
            op: step.opcode_name.clone().into(),
            gas: step.gas,
            gas_cost,
            depth: step.depth,
            error: None,
            stack: Some(step.stack.clone()),
            return_data: None,
            memory: step.memory.as_deref().map(encode_memory_words),
            memory_size: Some(step.memory_size as u64),
            storage: None,
            refund_counter: None,
        });
    }
    out
}

/// Re-encode raw EVM memory as the 32-byte big-endian hex words geth's
/// structLogger emits (and which `decode_struct_log_memory` parses back).
///
/// A trailing partial word is zero-padded on the right, which is what the
/// EVM's own word-addressed memory means: memory beyond `size` reads as
/// zero.
fn encode_memory_words(memory: &[u8]) -> Vec<String> {
    let mut words = Vec::with_capacity(memory.len().div_ceil(32));
    for chunk in memory.chunks(32) {
        let mut buf = [0u8; 32];
        buf[..chunk.len()].copy_from_slice(chunk);
        words.push(alloy::hex::encode(buf));
    }
    words
}

// ---------------------------------------------------------------------------
// solc selection
// ---------------------------------------------------------------------------

/// Pick a solc binary able to reproduce `full_version` (e.g.
/// `"0.8.28+commit.7893614a"`).
///
/// Search order, first hit wins:
///
/// 1. `SOLC_PATH` — an explicit choice by the caller always wins, even if it
///    is the wrong version; the recompile then fails loudly with solc's own
///    diagnostics, which is more useful than a silent substitution.
/// 2. `solc-<semver>` on `PATH` (the solc-select / nixpkgs multi-version
///    naming).
/// 3. `~/.svm/<semver>/solc-<semver>` (foundry's svm layout — populated if
///    the machine has ever built a project pinned to that release).
/// 4. `solc`.
///
/// Returns the command plus whether it was chosen BECAUSE of the version or
/// merely as the fallback, so the caller can say which happened.
pub fn resolve_solc_for_version(full_version: &str) -> (String, bool) {
    if let Ok(explicit) = std::env::var("SOLC_PATH")
        && !explicit.is_empty()
    {
        return (explicit, false);
    }

    let semver = source_fetcher::semver_prefix(full_version);
    if semver.is_empty() {
        return ("solc".to_string(), false);
    }

    let versioned = format!("solc-{semver}");
    if let Some(found) = which_on_path(&versioned) {
        return (found, true);
    }

    if let Some(home) = std::env::var_os("HOME") {
        let svm = PathBuf::from(home)
            .join(".svm")
            .join(semver)
            .join(&versioned);
        if svm.is_file() {
            return (svm.to_string_lossy().into_owned(), true);
        }
    }

    ("solc".to_string(), false)
}

/// Locate `name` on `PATH`, returning the full path when it is an existing
/// file.  Deliberately not `which(1)`: this runs inside a recorder that must
/// behave identically on a Nix shell and on a packaged install, and shelling
/// out to a tool that may not be installed is one more thing to diagnose.
fn which_on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Disassembly fallback
// ---------------------------------------------------------------------------

/// A disassembly listing of `bytecode`, plus a source map that points each
/// instruction at its own line of that listing.
///
/// This is what makes an unverified address traceable.  The recorder emits a
/// step only where the source map resolves a PC, so an address with no
/// artifacts contributes nothing; a listing is a source file the PCs can
/// resolve INTO, built from the deployed bytecode itself rather than
/// recovered from anywhere.
pub struct Disassembly {
    /// The listing text, one instruction per line.
    pub listing: String,
    /// A solc-shaped source map string: one `s:l:f:j:m` entry per
    /// instruction, in instruction order, pointing at that instruction's
    /// line in `listing`.
    pub source_map_raw: String,
    /// Number of instructions decoded.
    pub instructions: usize,
}

/// Disassemble EVM runtime bytecode into a listing and a matching source
/// map.
///
/// Each line is `<pc hex>  <MNEMONIC>` with the immediate appended for
/// `PUSH1`..`PUSH32`, so the listing reads as a program and a step's line
/// number identifies the instruction unambiguously.  Unknown opcode bytes
/// are rendered as `UNKNOWN_0x..` rather than skipped: they occupy a PC the
/// replay can step onto (data appended after a terminating instruction is
/// routinely not valid code), and a missing line would shift every line
/// after it.
///
/// The source map's entry for instruction `i` has `offset` = the byte offset
/// of line `i` in the listing and `length` = that line's length, which is
/// exactly how solc encodes a span; `SourceMap::resolve_pc` then turns it
/// into (line, column) with no special case anywhere else.
pub fn disassemble(bytecode: &[u8]) -> Disassembly {
    use revm::state::bytecode::opcode::OpCode;

    let mut listing = String::new();
    let mut entries: Vec<String> = Vec::new();
    let mut pc = 0usize;

    while pc < bytecode.len() {
        let opcode = bytecode[pc];
        let mnemonic = match OpCode::new(opcode) {
            Some(op) => format!("{op}"),
            None => format!("UNKNOWN_0x{opcode:02X}"),
        };
        // PUSH1 = 0x60 .. PUSH32 = 0x7f carry their immediate inline.
        let immediate_len = if (0x60..=0x7f).contains(&opcode) {
            (opcode - 0x60 + 1) as usize
        } else {
            0
        };
        let mut line = format!("{pc:#06x}  {mnemonic}");
        if immediate_len > 0 {
            let end = (pc + 1 + immediate_len).min(bytecode.len());
            let immediate = &bytecode[(pc + 1).min(bytecode.len())..end];
            line.push_str(&format!(" 0x{}", alloy::hex::encode(immediate)));
        }

        let offset = listing.len();
        entries.push(format!("{}:{}:0:-:0", offset, line.len()));
        listing.push_str(&line);
        listing.push('\n');

        pc += 1 + immediate_len;
    }

    Disassembly {
        listing,
        source_map_raw: entries.join(";"),
        instructions: entries.len(),
    }
}

/// Build [`ContractArtifacts`] for `address` from its deployed bytecode
/// alone, using [`disassemble`].
///
/// `source_paths` carries a relative name; `materialise_sources` writes the
/// listing under the trace directory and rewrites the path to where it
/// landed, the same way it does for recovered Solidity.
fn disassembly_artifacts(address: Address, bytecode: &[u8]) -> (ContractArtifacts, usize) {
    let disassembly = disassemble(bytecode);
    let artifacts = ContractArtifacts {
        name: format!("{address:#x}"),
        source_map: SourceMap::parse(&disassembly.source_map_raw),
        runtime_bytecode: bytecode.to_vec(),
        pc_to_idx: build_pc_to_instruction_index(bytecode),
        source_paths: vec![PathBuf::from(format!("{address:#x}.evmasm"))],
        source_contents: vec![disassembly.listing],
        storage_layout: None,
        solidity_ast: None,
    };
    (artifacts, disassembly.instructions)
}

// ---------------------------------------------------------------------------
// Public result / options
// ---------------------------------------------------------------------------

/// What happened to one address's source lookup.  Every executed address
/// gets a row, including the ones that failed, so the caller can print a
/// ledger instead of a success count.
#[derive(Debug, Clone)]
pub struct SourceLookup {
    pub address: Address,
    pub outcome: SourceOutcome,
    /// When the address ended up without verified source, how many
    /// instructions the disassembly listing registered in its place — the
    /// thing that keeps the container a recording rather than a header.
    /// `None` when verified source was used, or when the fallback was off.
    pub disassembly_instructions: Option<usize>,
}

/// The outcome of a single Sourcify lookup + recompile + bytecode check.
#[derive(Debug, Clone)]
pub enum SourceOutcome {
    /// Verified source recovered, recompiled, and the recompiled runtime
    /// bytecode agrees with `eth_getCode` at the target block.
    Mapped {
        contract_name: String,
        source_files: usize,
        /// Bytes of common prefix between recompiled and deployed code.
        prefix_match: usize,
        /// Length of the deployed runtime bytecode.
        deployed_len: usize,
    },
    /// Sourcify has no verified match for the address.
    NotVerified,
    /// Recompiled, but the result does not describe the deployed code.
    /// Dropped unless `allow_source_mismatch` was set.
    BytecodeMismatch {
        prefix_match: usize,
        recompiled_len: usize,
        deployed_len: usize,
        kept: bool,
    },
    /// Sourcify answered but the artifacts could not be produced (usually a
    /// solc version the machine does not have).  Carries the error chain.
    RecompileFailed { reason: String, solc: String },
    /// The address has no code at the target block (an EOA or a
    /// self-destructed contract); nothing to look up.
    NoCode,
}

/// Inputs to [`record_onchain_transaction`].
#[derive(Debug, Clone)]
pub struct OnchainOptions {
    /// JSON-RPC endpoint.  Must serve ARCHIVE state at the target block's
    /// parent — balance, nonce, code and storage at depth.  It does NOT
    /// need any `debug_*` or `trace_*` method.
    pub rpc_url: String,
    /// The transaction to trace.
    pub tx_hash: TxHash,
    /// Directory the CTFS container and the recovered sources go into.
    pub out_dir: PathBuf,
    /// Skip Sourcify entirely and record at EVM-opcode granularity.
    pub skip_source_fetch: bool,
    /// Keep recompiled artifacts whose bytecode disagrees with the deployed
    /// code.  Off by default: a misaligned source map attributes steps to
    /// the wrong lines, which is worse than no attribution.
    pub allow_source_mismatch: bool,
    /// Cap on distinct addresses looked up; see
    /// [`DEFAULT_MAX_SOURCE_LOOKUPS`].
    pub max_source_lookups: usize,
    /// Capture the executing frame's memory at every step.  On by default
    /// for this route: without it the recorder cannot decode `string` /
    /// `bytes` values or `LOG*` payloads.
    pub capture_memory: bool,
    /// For an address with no usable verified source, register a
    /// disassembly listing of its deployed bytecode and map every PC to
    /// the listing line for that instruction.
    ///
    /// On by default, and NOT cosmetic.  The recorder emits a step only
    /// where a source map resolves the PC to a source location, so an
    /// address with no artifacts contributes NO steps at all — a container
    /// written for an unverified mainnet contract held exactly the one step
    /// `TraceWriter::start` emits, against a replay that captured 556.
    /// "Recorded at EVM-opcode granularity" has to be made true by giving
    /// the opcodes somewhere to point.
    pub disassembly_fallback: bool,
}

impl OnchainOptions {
    /// Options for tracing `tx_hash` from `rpc_url` into `out_dir`, with the
    /// defaults this route is meant to run with.
    pub fn new(rpc_url: impl Into<String>, tx_hash: TxHash, out_dir: impl Into<PathBuf>) -> Self {
        Self {
            rpc_url: rpc_url.into(),
            tx_hash,
            out_dir: out_dir.into(),
            skip_source_fetch: false,
            allow_source_mismatch: false,
            max_source_lookups: DEFAULT_MAX_SOURCE_LOOKUPS,
            capture_memory: true,
            disassembly_fallback: true,
        }
    }
}

/// What [`record_onchain_transaction`] did, for the caller to report.
#[derive(Debug)]
pub struct OnchainRecording {
    pub chain_id: u64,
    pub block_number: u64,
    pub tx_index: usize,
    pub preceding_replayed: usize,
    pub spec_id: String,
    pub entry_point: Address,
    pub succeeded: bool,
    pub gas_used: u64,
    pub step_count: usize,
    pub call_count: usize,
    pub log_count: usize,
    pub memory_captured: bool,
    pub source_lookups: Vec<SourceLookup>,
    pub mapped_addresses: usize,
    pub out_dir: PathBuf,
    /// Path of the container that was written.
    pub container_path: PathBuf,
    /// Size of that container in bytes.
    pub container_bytes: u64,
    /// Steps the CURRENT reader sees after reopening the container.
    ///
    /// The route reopens what it wrote, through
    /// `codetracer_trace_writer_nim::NimTraceReaderHandle` — the same
    /// structured `ct_reader_*` FFI the db-backend consumes.  A container a
    /// current reader cannot open is not a recording, so this is part of
    /// the route rather than a check someone has to remember to run.
    pub container_steps_read_back: u64,
}

// ---------------------------------------------------------------------------
// The route
// ---------------------------------------------------------------------------

/// Trace one real on-chain transaction into a CTFS container in
/// `options.out_dir`.
///
/// Steps: replay the transaction against archive state (prestate strategy
/// `replay-preceding`), convert the inspector's output to structLogs,
/// recover what verified source is recoverable, materialise it next to the
/// container so the db-backend can resolve paths, then run the recorder's
/// multi-contract path and finalise.
pub async fn record_onchain_transaction(options: &OnchainOptions) -> Result<OnchainRecording> {
    std::fs::create_dir_all(&options.out_dir)
        .with_context(|| format!("cannot create output dir: {}", options.out_dir.display()))?;

    // ------------------------------------------------------------------ //
    // 1. Replay against archive state.                                    //
    // ------------------------------------------------------------------ //
    let replayed: ReplayedTransaction = replay_transaction_detailed(
        &options.rpc_url,
        options.tx_hash,
        ReplayOptions {
            capture_memory: options.capture_memory,
        },
    )
    .await
    .with_context(|| {
        format!(
            "replay of {} against {} failed",
            options.tx_hash, options.rpc_url
        )
    })?;

    if replayed.execution.is_empty() {
        return Err(eyre::eyre!(
            "the replay of {} captured 0 EVM steps — a plain value transfer to an \
             account with no code has no execution to trace",
            options.tx_hash
        ));
    }

    // A contract creation has no `to`; the recorder's registry is keyed by
    // address and the created address is not known until the frame returns,
    // so the entry point is reported as the zero address and the creation
    // records unmapped.
    let entry_point = replayed.to.unwrap_or(Address::ZERO);

    // ------------------------------------------------------------------ //
    // 2. Convert to the structLog shape the recorder consumes.            //
    // ------------------------------------------------------------------ //
    let struct_logs = execution_data_to_struct_logs(&replayed.execution);

    // ------------------------------------------------------------------ //
    // 3. Recover verified source for the addresses that executed.         //
    // ------------------------------------------------------------------ //
    let addresses = executed_addresses(&replayed, options.max_source_lookups);
    let mut registry = ContractRegistry::new();
    let mut lookups = Vec::with_capacity(addresses.len());
    let mut mapped = 0usize;

    // Same provider construction as the replay, so these reads get the same
    // rate-limit retry.
    let provider = build_provider(&options.rpc_url)?;
    for address in &addresses {
        let (outcome, disassembly_instructions) = resolve_source_for_address(
            &provider,
            *address,
            replayed.chain_id,
            replayed.block_number,
            options,
            &mut registry,
        )
        .await;
        if matches!(outcome, SourceOutcome::Mapped { .. })
            || matches!(outcome, SourceOutcome::BytecodeMismatch { kept: true, .. })
        {
            mapped += 1;
        }
        lookups.push(SourceLookup {
            address: *address,
            outcome,
            disassembly_instructions,
        });
    }

    // ------------------------------------------------------------------ //
    // 4. Materialise the recovered sources next to the container, and     //
    //    point the artifacts at them.                                     //
    // ------------------------------------------------------------------ //
    materialise_sources(&mut registry, &addresses, &options.out_dir)?;

    if registry.get(&entry_point).is_some() {
        registry.set_default(entry_point);
    }

    // ------------------------------------------------------------------ //
    // 5. Record.                                                          //
    // ------------------------------------------------------------------ //
    // `metadata.program` carries the transaction identity rather than a
    // file path: there is no local program here, and the cross-recorder
    // path-as-program convention has nothing to name.  The chain id and
    // block are included so a container is self-describing without its
    // sidecar.
    //
    // Only `-` separates the parts.  The Nim writer names the container
    // `<program>.ct`, and `:` is not a legal filename character on Windows
    // — which this recorder is packaged for (see `packaging/scoop`,
    // `packaging/chocolatey`).
    let program_label = format!(
        "evm-{}-{}-{}",
        replayed.chain_id, options.tx_hash, replayed.block_number
    );
    let mut recorder = EvmRecorder::new(&program_label, &options.out_dir)
        .context("failed to create EvmRecorder")?;
    recorder
        .initialize()
        .context("failed to initialize EvmRecorder")?;

    recorder
        .record_from_structlog_multi_contract(&struct_logs, &registry, entry_point)
        .context("recorder failed to process the replayed structlogs")?;

    if !replayed.succeeded {
        let decoded = revert_decode::decode_revert_with_registry(
            &replayed.output,
            &revert_decode::CustomErrorRegistry::default(),
        );
        recorder.register_revert(decoded.kind, &decoded.message);
    }

    recorder
        .finalize()
        .context("failed to finalize EvmRecorder")?;

    // ------------------------------------------------------------------ //
    // 6. Reopen what was written, through a current reader.               //
    // ------------------------------------------------------------------ //
    // The Nim writer names the container `<out_dir>/<program>.ct`.
    let container_path = options.out_dir.join(format!("{program_label}.ct"));
    let container_bytes = std::fs::metadata(&container_path)
        .with_context(|| {
            format!(
                "the recorder reported success but wrote no container at {}",
                container_path.display()
            )
        })?
        .len();
    let reader = NimTraceReaderHandle::open(
        container_path
            .to_str()
            .ok_or_else(|| eyre::eyre!("container path is not valid UTF-8"))?,
    )
    .map_err(|e| {
        eyre::eyre!(
            "the container at {} was written but the current reader refused it: {e}",
            container_path.display()
        )
    })?;
    let container_steps_read_back = reader.step_count();
    if container_steps_read_back == 0 {
        return Err(eyre::eyre!(
            "the container at {} opened but the reader sees 0 steps, while the \
             replay captured {} — the container is not a recording of this \
             transaction",
            container_path.display(),
            replayed.execution.steps.len(),
        ));
    }

    Ok(OnchainRecording {
        chain_id: replayed.chain_id,
        block_number: replayed.block_number,
        tx_index: replayed.tx_index,
        preceding_replayed: replayed.preceding_replayed,
        spec_id: format!("{:?}", replayed.spec_id),
        entry_point,
        succeeded: replayed.succeeded,
        gas_used: replayed.gas_used,
        step_count: replayed.execution.steps.len(),
        call_count: replayed.execution.calls.len(),
        log_count: replayed.execution.logs.len(),
        memory_captured: options.capture_memory,
        source_lookups: lookups,
        mapped_addresses: mapped,
        out_dir: options.out_dir.clone(),
        container_path,
        container_bytes,
        container_steps_read_back,
    })
}

/// The distinct addresses whose code executed, entry point first, capped at
/// `limit`.
///
/// Order matters: the entry point's artifacts decide the container's main
/// source path, so it is looked up first and the cap can only cost deeper
/// frames.
fn executed_addresses(replayed: &ReplayedTransaction, limit: usize) -> Vec<Address> {
    let mut ordered: Vec<Address> = Vec::new();
    let mut seen: BTreeSet<Address> = BTreeSet::new();

    if let Some(to) = replayed.to
        && seen.insert(to)
    {
        ordered.push(to);
    }
    for call in &replayed.execution.calls {
        if call.target != Address::ZERO && seen.insert(call.target) {
            ordered.push(call.target);
        }
        if ordered.len() >= limit {
            break;
        }
    }
    ordered.truncate(limit);
    ordered
}

/// Resolve one address's artifacts: verified source where it is usable, a
/// disassembly listing otherwise, and register whichever won.
///
/// Returns the lookup outcome plus the instruction count of the disassembly
/// listing when the fallback was what got registered, so the caller can say
/// which kind of recording this address contributed.
async fn resolve_source_for_address<P: Provider>(
    provider: &P,
    address: Address,
    chain_id: u64,
    block_number: u64,
    options: &OnchainOptions,
    registry: &mut ContractRegistry,
) -> (SourceOutcome, Option<usize>) {
    let deployed = match provider
        .get_code_at(address)
        .block_id(block_number.into())
        .await
    {
        Ok(code) => code,
        Err(err) => {
            // Without the deployed code there is no bytecode check and no
            // listing to fall back to, so this address stays unmapped.
            return (
                SourceOutcome::RecompileFailed {
                    reason: format!("eth_getCode at block {block_number} failed: {err}"),
                    solc: String::new(),
                },
                None,
            );
        }
    };
    if deployed.is_empty() {
        return (SourceOutcome::NoCode, None);
    }

    // Registers the disassembly listing and reports how many instructions it
    // holds.  Used for every path below that does not end in usable verified
    // source.
    let fall_back = |registry: &mut ContractRegistry| -> Option<usize> {
        if !options.disassembly_fallback {
            return None;
        }
        let (artifacts, instructions) = disassembly_artifacts(address, &deployed);
        registry.register(address, artifacts);
        Some(instructions)
    };

    if options.skip_source_fetch {
        let instructions = fall_back(registry);
        return (SourceOutcome::NotVerified, instructions);
    }

    let bundle = match source_fetcher::fetch_sourcify_files(chain_id, address).await {
        Ok(Some(bundle)) => bundle,
        Ok(None) => {
            let instructions = fall_back(registry);
            return (SourceOutcome::NotVerified, instructions);
        }
        Err(err) => {
            let instructions = fall_back(registry);
            return (
                SourceOutcome::RecompileFailed {
                    reason: format!("Sourcify lookup failed: {err:#}"),
                    solc: String::new(),
                },
                instructions,
            );
        }
    };

    // Choose the solc that can reproduce the verified artifacts.
    let full_version = bundle
        .metadata_json
        .as_deref()
        .and_then(|m| source_fetcher::parse_metadata_settings(m).ok())
        .map(|s| s.solc_version)
        .unwrap_or_default();
    let (solc, version_matched) = resolve_solc_for_version(&full_version);

    let artifacts = match source_fetcher::compile_sourcify_bundle(&bundle, &solc, None) {
        Ok(artifacts) => artifacts,
        Err(err) => {
            let instructions = fall_back(registry);
            return (
                SourceOutcome::RecompileFailed {
                    reason: format!(
                        "recompile with `{solc}` failed (contract was verified with \
                         `{full_version}`; a version-specific binary was \
                         {}): {err:#}",
                        if version_matched {
                            "found"
                        } else {
                            "NOT found, so this is the fallback solc"
                        }
                    ),
                    solc,
                },
                instructions,
            );
        }
    };

    let prefix_match = common_prefix_len(&artifacts.runtime_bytecode, &deployed);
    let aligned = prefix_match == artifacts.runtime_bytecode.len().min(deployed.len());

    if aligned {
        let outcome = SourceOutcome::Mapped {
            contract_name: artifacts.name.clone(),
            source_files: artifacts.source_paths.len(),
            prefix_match,
            deployed_len: deployed.len(),
        };
        registry.register(address, artifacts);
        return (outcome, None);
    }

    let recompiled_len = artifacts.runtime_bytecode.len();
    let mut instructions = None;
    if options.allow_source_mismatch {
        registry.register(address, artifacts);
    } else {
        instructions = fall_back(registry);
    }
    (
        SourceOutcome::BytecodeMismatch {
            prefix_match,
            recompiled_len,
            deployed_len: deployed.len(),
            kept: options.allow_source_mismatch,
        },
        instructions,
    )
}

/// Length of the common prefix of two byte strings.
///
/// Used rather than equality because solc is invoked with
/// `--no-cbor-metadata`, so a correct recompile is a PREFIX of the deployed
/// code (which carries the metadata hash solc appends) rather than equal to
/// it.
fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Write every registered contract's recovered sources under
/// `out_dir/sources/` and rewrite the artifacts' `source_paths` to point
/// there.
///
/// Sourcify reports repo-relative paths like
/// `/contracts/full_match/1/0x…/sources/Token.sol`, which exist nowhere on
/// this machine.  The db-backend resolves a trace's source paths from the
/// container's own `paths` stream, so the files have to exist — and the
/// order of `source_paths` has to be preserved exactly, because the source
/// map's `file_index` indexes into it.
fn materialise_sources(
    registry: &mut ContractRegistry,
    addresses: &[Address],
    out_dir: &Path,
) -> Result<()> {
    let sources_root = out_dir.join("sources");
    for address in addresses {
        let Some(artifacts) = registry.get_mut(address) else {
            continue;
        };
        let per_contract = sources_root.join(format!("{address:#x}"));
        let mut rewritten: Vec<PathBuf> = Vec::with_capacity(artifacts.source_paths.len());
        for (index, original) in artifacts.source_paths.iter().enumerate() {
            let target = per_contract.join(sanitise_source_path(original, index));
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create source dir: {}", parent.display()))?;
            }
            let contents = artifacts
                .source_contents
                .get(index)
                .map(String::as_str)
                .unwrap_or("");
            std::fs::write(&target, contents)
                .with_context(|| format!("cannot write source file: {}", target.display()))?;
            rewritten.push(target);
        }
        artifacts.source_paths = rewritten;
    }
    Ok(())
}

/// Turn a Sourcify repo-relative path into a safe relative path under the
/// trace directory.
///
/// Leading separators and `..` components are dropped — a remote service
/// names these files, and a path it supplies must not be able to select a
/// write target outside the trace directory.  A path that sanitises to
/// nothing falls back to `source_<index>.sol`, which keeps the `file_index`
/// correspondence intact.
fn sanitise_source_path(original: &Path, index: usize) -> PathBuf {
    let mut safe = PathBuf::new();
    for component in original.components() {
        if let std::path::Component::Normal(part) = component
            && part != ".."
        {
            safe.push(part);
        }
    }
    if safe.as_os_str().is_empty() {
        safe.push(format!("source_{index}.sol"));
    }
    safe
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspector::StepData;
    use alloy::primitives::U256;

    fn step(pc: usize, op: &str, depth: u64, gas: u64, memory: Option<Vec<u8>>) -> StepData {
        StepData {
            pc,
            opcode: 0x00,
            opcode_name: op.to_string(),
            depth,
            stack: vec![U256::from(1u64), U256::from(2u64)],
            memory_size: memory.as_ref().map(|m| m.len()).unwrap_or(0),
            gas,
            memory,
        }
    }

    #[test]
    fn struct_logs_carry_every_field_the_recorder_reads() {
        let data = ExecutionData {
            steps: vec![
                step(0, "PUSH1", 1, 1000, Some(vec![0xaa; 32])),
                step(2, "MSTORE", 1, 997, Some(vec![0xaa; 32])),
            ],
            ..ExecutionData::default()
        };
        let logs = execution_data_to_struct_logs(&data);
        assert_eq!(logs.len(), 2);

        assert_eq!(logs[0].pc, 0);
        assert_eq!(logs[0].op.as_ref(), "PUSH1");
        assert_eq!(logs[0].depth, 1);
        assert_eq!(logs[0].gas, 1000);
        // Same depth, so the cost is the gas difference to the next step.
        assert_eq!(logs[0].gas_cost, 3);
        assert_eq!(
            logs[0].stack.as_deref(),
            Some(&[U256::from(1u64), U256::from(2u64)][..])
        );
        assert_eq!(
            logs[0].memory.as_deref(),
            Some(
                &["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()][..]
            )
        );
        assert_eq!(logs[0].memory_size, Some(32));
        assert_eq!(logs[0].storage, None);
        assert_eq!(logs[0].return_data, None);
        assert_eq!(logs[0].refund_counter, None);
        assert_eq!(logs[0].error, None);

        // Last step has no successor, so no cost can be derived.
        assert_eq!(logs[1].gas_cost, 0);
    }

    #[test]
    fn gas_cost_is_not_derived_across_a_frame_boundary() {
        // Step 0 is the CALL at depth 1; step 1 is the callee's first
        // opcode at depth 2 with its own, much smaller, gas budget.  The
        // difference between the two is not an opcode cost.
        let data = ExecutionData {
            steps: vec![
                step(10, "CALL", 1, 100_000, None),
                step(0, "PUSH1", 2, 2_300, None),
            ],
            ..ExecutionData::default()
        };
        let logs = execution_data_to_struct_logs(&data);
        assert_eq!(logs[0].gas_cost, 0);
    }

    #[test]
    fn a_partial_memory_word_is_right_padded_with_zeroes() {
        // The EVM's memory is word-addressed; bytes past `size` read as
        // zero, so a 33-byte memory is two words and the second is
        // 0x01 followed by 31 zero bytes.
        let words = encode_memory_words(&[vec![0xff; 32], vec![0x01]].concat());
        assert_eq!(words.len(), 2);
        assert_eq!(
            words[0],
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        );
        assert_eq!(
            words[1],
            "0100000000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn memory_is_absent_rather_than_empty_when_it_was_not_captured() {
        // An absent `memory` and an empty `memory` mean different things:
        // "not captured" versus "the frame's memory is zero bytes long".
        // The conversion must not collapse them.
        let data = ExecutionData {
            steps: vec![
                step(0, "STOP", 1, 10, None),
                step(0, "STOP", 1, 10, Some(vec![])),
            ],
            ..ExecutionData::default()
        };
        let logs = execution_data_to_struct_logs(&data);
        assert_eq!(logs[0].memory, None);
        assert_eq!(logs[1].memory, Some(vec![]));
    }

    #[test]
    fn a_disassembly_listing_has_one_line_per_instruction() {
        // PUSH1 0x80, PUSH1 0x40, MSTORE, STOP — 4 instructions over 7
        // bytes, so an instruction count equal to the byte count would be
        // wrong.
        let bytecode = vec![0x60, 0x80, 0x60, 0x40, 0x52, 0x00];
        let d = disassemble(&bytecode);
        assert_eq!(d.instructions, 4);
        let lines: Vec<&str> = d.listing.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "0x0000  PUSH1 0x80");
        assert_eq!(lines[1], "0x0002  PUSH1 0x40");
        assert_eq!(lines[2], "0x0004  MSTORE");
        assert_eq!(lines[3], "0x0005  STOP");
    }

    #[test]
    fn every_pc_resolves_to_its_own_instruction_line() {
        // This is the property the whole fallback exists for: the recorder
        // emits a step only where the source map RESOLVES a PC, so each
        // instruction's PC must land on its own listing line.
        let bytecode = vec![0x60, 0x80, 0x60, 0x40, 0x52, 0x00];
        let d = disassemble(&bytecode);
        let source_map = SourceMap::parse(&d.source_map_raw);
        let pc_to_idx = build_pc_to_instruction_index(&bytecode);
        let listing = d.listing.clone();
        let contents = [listing.as_str()];

        let resolved: Vec<u32> = [0usize, 2, 4, 5]
            .iter()
            .map(|&pc| {
                source_map
                    .resolve_pc(pc, &pc_to_idx, &contents)
                    .unwrap_or_else(|| panic!("pc {pc} must resolve"))
                    .line
            })
            .collect();
        assert_eq!(
            resolved,
            vec![1, 2, 3, 4],
            "the four instruction PCs must map to the four listing lines in order"
        );
    }

    #[test]
    fn an_unknown_opcode_byte_still_occupies_a_line() {
        // Bytes appended after a terminating instruction are routinely not
        // valid code, and the replay can step onto them.  Skipping one
        // would shift every line after it, so the mapping has to keep the
        // slot.
        let bytecode = vec![0x00, 0x0c, 0x5b];
        let d = disassemble(&bytecode);
        assert_eq!(d.instructions, 3);
        let lines: Vec<&str> = d.listing.lines().collect();
        assert_eq!(lines[1], "0x0001  UNKNOWN_0x0C");
        assert_eq!(lines[2], "0x0002  JUMPDEST");
    }

    #[test]
    fn a_truncated_push_immediate_does_not_panic_or_lose_the_instruction() {
        // PUSH32 with no immediate bytes at all — the tail of a contract
        // whose code was cut, which `eth_getCode` can legitimately return.
        let bytecode = vec![0x7f];
        let d = disassemble(&bytecode);
        assert_eq!(d.instructions, 1);
        assert_eq!(d.listing.lines().next().unwrap(), "0x0000  PUSH32 0x");
    }

    #[test]
    fn disassembly_artifacts_carry_the_deployed_bytecode_and_one_source() {
        let bytecode = vec![0x60, 0x01, 0x00];
        let address = Address::from([0x11u8; 20]);
        let (artifacts, instructions) = disassembly_artifacts(address, &bytecode);
        assert_eq!(instructions, 2);
        assert_eq!(artifacts.runtime_bytecode, bytecode);
        assert_eq!(artifacts.source_paths.len(), 1);
        assert_eq!(artifacts.source_contents.len(), 1);
        assert_eq!(
            artifacts.source_paths[0],
            PathBuf::from("0x1111111111111111111111111111111111111111.evmasm")
        );
        // No storage layout or AST can be recovered from bytecode, and
        // claiming either would be a fabrication.
        assert!(artifacts.storage_layout.is_none());
        assert!(artifacts.solidity_ast.is_none());
    }

    #[test]
    fn a_sourcify_absolute_path_is_confined_to_the_trace_directory() {
        let safe = sanitise_source_path(
            Path::new("/contracts/full_match/1/0xabc/sources/Token.sol"),
            0,
        );
        assert_eq!(
            safe,
            PathBuf::from("contracts/full_match/1/0xabc/sources/Token.sol")
        );
        assert!(safe.is_relative());
    }

    #[test]
    fn a_dot_dot_escape_in_a_remote_path_is_dropped() {
        let safe = sanitise_source_path(Path::new("/../../etc/passwd"), 3);
        assert_eq!(safe, PathBuf::from("etc/passwd"));
    }

    #[test]
    fn an_empty_remote_path_falls_back_to_its_file_index() {
        // The fallback must keep one entry per index: the source map's
        // file_index indexes into source_paths positionally.
        let safe = sanitise_source_path(Path::new("/"), 2);
        assert_eq!(safe, PathBuf::from("source_2.sol"));
    }

    #[test]
    fn a_no_cbor_metadata_recompile_is_a_prefix_of_the_deployed_code() {
        // solc is invoked with --no-cbor-metadata, so a correct recompile
        // is shorter than the deployed code by the metadata solc appends.
        let recompiled = vec![0x60, 0x80, 0x60, 0x40];
        let deployed = vec![0x60, 0x80, 0x60, 0x40, 0xa2, 0x64];
        assert_eq!(common_prefix_len(&recompiled, &deployed), 4);
        assert_eq!(
            common_prefix_len(&recompiled, &deployed),
            recompiled.len().min(deployed.len())
        );
    }

    #[test]
    fn a_different_contract_is_not_a_prefix() {
        let recompiled = vec![0x60, 0x80, 0x60, 0x40];
        let deployed = vec![0x60, 0x80, 0x52, 0x34];
        assert_eq!(common_prefix_len(&recompiled, &deployed), 2);
        assert_ne!(
            common_prefix_len(&recompiled, &deployed),
            recompiled.len().min(deployed.len())
        );
    }

    #[test]
    fn solc_path_overrides_the_version_search() {
        // Covers the precedence rule, not the filesystem: an explicit
        // SOLC_PATH is honoured verbatim and reported as NOT
        // version-matched, so the caller can say the recompile ran under a
        // binary it did not choose.
        //
        // Single test for the env var: `std::env::set_var` is process-wide
        // and Rust runs tests in threads, so two tests mutating SOLC_PATH
        // would race.
        let previous = std::env::var_os("SOLC_PATH");
        unsafe { std::env::set_var("SOLC_PATH", "/opt/custom/solc") };
        let (cmd, matched) = resolve_solc_for_version("0.8.28+commit.7893614a");
        assert_eq!(cmd, "/opt/custom/solc");
        assert!(!matched);

        unsafe { std::env::remove_var("SOLC_PATH") };
        let (fallback, fallback_matched) = resolve_solc_for_version("0.4.18+commit.9cf6e910");
        // No `solc-0.4.18` is expected on a test machine; the point is that
        // the fallback is reported as a fallback rather than as a match.
        if !fallback_matched {
            assert_eq!(fallback, "solc");
        }

        match previous {
            Some(value) => unsafe { std::env::set_var("SOLC_PATH", value) },
            None => unsafe { std::env::remove_var("SOLC_PATH") },
        }
    }
}
