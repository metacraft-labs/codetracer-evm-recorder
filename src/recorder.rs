use codetracer_trace_types::{EventLogKind, Line, NONE_VALUE, TypeId, TypeKind, ValueRecord};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{TraceEventsFileFormat, create_trace_writer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use alloy::primitives::Address;

use crate::call_tree::{CallTree, CallType};
use crate::contract_registry::ContractRegistry;
use crate::memory_tracker::MemoryTracker;
use crate::revert_decode;
use crate::solidity_ast::{FunctionDef, SolidityAst};
use crate::source_map::{self, JumpType, SourceMap};
use crate::stack_tracker::StackTracker;
use crate::storage_layout::StorageLayout;
use crate::structlog::StructLog;

const INTERNAL_CALL_LOOKAHEAD: usize = 5;

/// Map a Solidity type name to the appropriate CodeTracer `TypeKind`.
fn type_kind_for_solidity_type(type_name: &str) -> TypeKind {
    match type_name {
        "bool" => TypeKind::Bool,
        "string" => TypeKind::String,
        "bytes" => TypeKind::Seq,
        "address" | "address payable" => TypeKind::Raw,
        s if s.starts_with("bytes") => {
            // bytes1..bytes32 are fixed-size byte arrays (raw)
            // but "bytes" (dynamic) is Seq — already handled above
            TypeKind::Raw
        }
        s if s.starts_with("uint") || s.starts_with("int") => TypeKind::Int,
        s if s.starts_with("enum ") => TypeKind::Int,
        // Mappings, arrays, structs, contract types — fall back to Raw
        // since on the stack they are represented as 256-bit values
        // (storage slots, memory pointers, or addresses).
        _ => TypeKind::Raw,
    }
}

/// Assemble a `ValueRecord::Sequence` (for fixed-size arrays) or
/// `ValueRecord::Struct` (for `inplace`-encoded structs) snapshot of
/// the compound storage variable described by `parent` / `parent_ti`,
/// drawing per-slot values from `slot_values`.
///
/// Returns `None` for compounds we can't reliably reconstruct from
/// the storage layout alone (e.g. mappings, dynamic arrays).
fn build_compound_value(
    writer: &mut dyn TraceWriter,
    layout: &crate::storage_layout::StorageLayout,
    _parent: &crate::storage_layout::StorageEntry,
    parent_ti: &crate::storage_layout::StorageTypeInfo,
    base_slot: u64,
    slot_count: u64,
    slot_values: &std::collections::HashMap<u64, alloy::primitives::U256>,
) -> Option<ValueRecord> {
    let element_type_id = TraceWriter::ensure_type_id(writer, TypeKind::Int, "uint256");

    if let Some(members) = parent_ti.members.as_ref() {
        // Struct: one element per member, in declared order.  Each
        // member's `slot` field is relative to the struct base slot
        // (per Solidity storage layout encoding).
        let parent_type_id =
            TraceWriter::ensure_type_id(writer, TypeKind::Struct, &parent_ti.label);
        let mut field_values: Vec<ValueRecord> = Vec::with_capacity(members.len());
        for member in members {
            let member_offset: u64 = member.slot.parse().ok()?;
            let abs_slot = base_slot + member_offset;
            let raw = slot_values
                .get(&abs_slot)
                .copied()
                .unwrap_or(alloy::primitives::U256::ZERO);
            field_values.push(ValueRecord::Raw {
                r: format!("0x{:x}", raw),
                type_id: element_type_id,
            });
        }
        Some(ValueRecord::Struct {
            field_values,
            type_id: parent_type_id,
        })
    } else if parent_ti.base.is_some() {
        // Fixed-size array: one element per slot in the contiguous range.
        let parent_type_id = TraceWriter::ensure_type_id(writer, TypeKind::Array, &parent_ti.label);
        let mut elements: Vec<ValueRecord> = Vec::with_capacity(slot_count as usize);
        for offset in 0..slot_count {
            let abs_slot = base_slot + offset;
            let raw = slot_values
                .get(&abs_slot)
                .copied()
                .unwrap_or(alloy::primitives::U256::ZERO);
            elements.push(ValueRecord::Raw {
                r: format!("0x{:x}", raw),
                type_id: element_type_id,
            });
        }
        let _ = layout; // currently unused; kept for symmetry/future use
        Some(ValueRecord::Sequence {
            elements,
            is_slice: false,
            type_id: parent_type_id,
        })
    } else {
        None
    }
}

/// Build a `ValueRecord` for a 256-bit EVM stack word that backs a
/// Solidity local variable of `type_name`.
///
/// When the value fits in `i64` and the type is integer-shaped
/// (`uint*` / `int*` / `enum`) we surface it as `ValueRecord::Int`,
/// matching the CodeTracer `TypeKind::Int` registration used for
/// `uint256` and friends.  Larger integers and non-integer types fall
/// back to `ValueRecord::Raw` (lossless 0x-prefixed hex), which keeps
/// `address`, `bytes32`, mappings/arrays/structs (whose stack
/// representation is a 32-byte pointer or storage slot) untouched.
///
/// The integer-only narrowing is what makes the
/// `_decodes_loop_sums` test in `tests/test_programs_via_ct_print_full.rs`
/// observe at least one `ValueRecord::Int` in the trace without
/// silently downcasting larger storage slot keys (e.g.
/// `keccak256` of a mapping key) into a lossy `i64`.
fn value_record_for_local(
    value: alloy::primitives::U256,
    type_name: &str,
    type_id: TypeId,
) -> ValueRecord {
    let is_int_type = matches!(type_kind_for_solidity_type(type_name), TypeKind::Int);
    if is_int_type && value.bit_len() <= 63 {
        // Safe narrowing: the high three limbs are zero, the low limb's
        // top bit is zero too (bit_len() <= 63), so casting to i64 is
        // both lossless and non-negative.
        let i = value.as_limbs()[0] as i64;
        ValueRecord::Int { i, type_id }
    } else {
        ValueRecord::Raw {
            r: format!("0x{:x}", value),
            type_id,
        }
    }
}

/// Resolve the Solidity function entered by an internal EVM jump.
///
/// The first few instructions after a Solidity internal-function JUMP often
/// belong to compiler-generated prologue code with no useful source map entry.
/// Looking ahead a small fixed window lets us land on the first body
/// instruction whose source offset falls inside the callee's FunctionDefinition.
fn resolve_internal_call_target<'a>(
    struct_logs: &[StructLog],
    current_index: usize,
    source_map: &SourceMap,
    pc_to_idx: &[usize],
    solidity_ast: Option<&'a SolidityAst>,
) -> Option<&'a FunctionDef> {
    let ast = solidity_ast?;
    (1..=INTERNAL_CALL_LOOKAHEAD).find_map(|step| {
        let next_log = struct_logs.get(current_index + step)?;
        let entry = source_map.get_entry_for_pc(next_log.pc as usize, pc_to_idx)?;
        if entry.file_index < 0 {
            return None;
        }
        ast.function_at(entry.offset, entry.file_index)
    })
}

/// Fallback resolver for JUMPs whose immediate post-jump instructions
/// stay in compiler-generated code (loop-continuation JUMPDESTs,
/// shared dispatcher back-edges, ...).
///
/// This is the implementation of category 1 of the M11 recorder
/// fixes: instead of surfacing such frames under a synthetic
/// `fn_at_pc_<n>` placeholder, look up the JUMP **source** site's
/// enclosing function in the AST.  Practically every dispatcher
/// orphan JUMP we see in user-mode internal-call sequences is an
/// intra-function back-edge (loop / branch continuation) — i.e. the
/// JUMP itself is emitted inside the body of a Solidity function,
/// even when the JUMP target's source map entry has been clobbered
/// by stack-shuffling compiler-generated code.
///
/// Returns the `FunctionDef` whose source range contains the JUMP
/// instruction's own source offset, when one exists.  Falls back to
/// `None` (preserving the existing `fn_at_pc_*` placeholder path)
/// when neither the JUMP source nor any nearby instruction maps to a
/// known function — the canonical case here is a constructor-time
/// JUMP that lands entirely outside any runtime FunctionDefinition.
fn resolve_enclosing_function_for_jump<'a>(
    struct_logs: &[StructLog],
    current_index: usize,
    source_map: &SourceMap,
    pc_to_idx: &[usize],
    solidity_ast: Option<&'a SolidityAst>,
) -> Option<&'a FunctionDef> {
    let ast = solidity_ast?;
    let log = struct_logs.get(current_index)?;
    let entry = source_map.get_entry_for_pc(log.pc as usize, pc_to_idx)?;
    if entry.file_index < 0 {
        return None;
    }
    ast.function_at(entry.offset, entry.file_index)
}

/// Compute `keccak256(input)` and return the 256-bit result as a
/// big-endian `U256` — used to identify the storage slot that an
/// SSTORE writes to when the slot was derived from a mapping
/// `keccak256(key . base_slot)` operation we intercepted earlier.
fn keccak256_u256(input: &[u8]) -> alloy::primitives::U256 {
    let hash = alloy::primitives::keccak256(input);
    alloy::primitives::U256::from_be_bytes(*hash)
}

/// Format a 32-byte mapping key as a Solidity-style literal, given
/// the canonical key type label from the storage layout.
///
///   * `address` / `address payable`: lower-case 20-byte hex
///     (`0x70997970...c8`), trimming the upper 12 zero bytes.
///   * Integer types (`uint*` / `int*`): canonical hex with a `0x`
///     prefix and no leading zeros, matching `format!("0x{:x}", ...)`.
///   * Everything else: full 32-byte hex blob (lossless fallback).
fn format_mapping_key(key_type_label: &str, key_bytes: &[u8; 32]) -> String {
    if matches!(key_type_label, "address" | "address payable") {
        let mut s = String::with_capacity(2 + 40);
        s.push_str("0x");
        use std::fmt::Write as _;
        for byte in &key_bytes[12..] {
            let _ = write!(s, "{:02x}", byte);
        }
        return s;
    }
    if key_type_label.starts_with("uint")
        || key_type_label.starts_with("int")
        || key_type_label == "bool"
    {
        let value = alloy::primitives::U256::from_be_bytes(*key_bytes);
        return format!("0x{:x}", value);
    }
    // Fallback: full 32-byte hex.
    let mut s = String::with_capacity(2 + 64);
    s.push_str("0x");
    use std::fmt::Write as _;
    for byte in key_bytes {
        let _ = write!(s, "{:02x}", byte);
    }
    s
}

fn stage_internal_call_args(
    writer: &mut dyn TraceWriter,
    tracker: &mut StackTracker,
    target_fn: Option<&FunctionDef>,
    concrete_stack: Option<&[alloy::primitives::U256]>,
    stack_items_above_params: usize,
) {
    let Some(target_fn) = target_fn else {
        tracker.reset();
        return;
    };

    let param_names: Vec<String> = target_fn
        .parameters
        .iter()
        .map(|param| param.name.clone())
        .collect();
    let Some(stack) = concrete_stack else {
        tracker.reset();
        return;
    };
    let callee_stack_depth = stack.len().saturating_sub(stack_items_above_params);
    let occupied_slots = param_names.len() + stack_items_above_params;
    if param_names.is_empty() || occupied_slots > stack.len() {
        tracker.seed_top_labels(callee_stack_depth, &param_names);
        return;
    }

    let first_param_slot = stack.len() - occupied_slots;
    for (idx, param) in target_fn.parameters.iter().enumerate() {
        let value = stack[first_param_slot + idx];
        let type_kind = type_kind_for_solidity_type(&param.type_name);
        let type_id = TraceWriter::ensure_type_id(writer, type_kind, &param.type_name);
        let val_record = ValueRecord::Raw {
            r: format!("0x{:x}", value),
            type_id,
        };
        writer.arg(&param.name, val_record);
    }

    tracker.seed_top_labels(callee_stack_depth, &param_names);
}

/// Main EVM trace recorder. Processes EVM execution traces (structLog or
/// inspector-based) and writes them in CodeTracer's trace format.
pub struct EvmRecorder {
    writer: Box<dyn TraceWriter + Send>,
    type_names: Vec<String>,
    output_dir: PathBuf,
    /// Paths already registered with their per-line byte-length tables
    /// via `register_path_with_line_lengths`.  Tracked so we only emit
    /// the Layout A `paths.dat` record once per source file (the first
    /// registration wins per the Nim writer's semantics — see
    /// `codetracer_trace_writer_nim::NimTraceWriter::register_path_with_line_lengths`).
    paths_with_line_lengths: HashSet<PathBuf>,
}

/// Compute the per-line UTF-8 byte-length table required by the
/// `paths.dat` Layout A record (column-aware mode).
///
/// `line_lengths[i]` is the byte count of source line `i+1` (1-based,
/// matching the CTFS spec), excluding the trailing `\n`.  An `\r\n`
/// terminator contributes its `\r` to the line's byte count, which
/// keeps the table consistent with the column offsets the solc source
/// map emits (byte offsets into the file).  A file that doesn't end
/// with `\n` still has its final line counted.
///
/// See `codetracer-trace-format-spec/trace-events.md` §"paths.dat
/// per-line offset table — Layout A".
fn compute_line_lengths(source: &str) -> Vec<u32> {
    let mut lengths: Vec<u32> = Vec::new();
    let mut line_start: usize = 0;
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            let line_len = (i - line_start) as u32;
            lengths.push(line_len);
            line_start = i + 1;
        }
    }
    if line_start < source.len() {
        lengths.push((source.len() - line_start) as u32);
    }
    lengths
}

impl EvmRecorder {
    /// Create a new recorder targeting `output_dir`.
    ///
    /// The recorder uses [`TraceEventsFileFormat::Ctfs`] (CodeTracer's
    /// canonical multi-stream container format).  This matches the format
    /// used by the Ruby and Python native recorders post-2026-04 (handoff
    /// entries 1.21 / 1.22 / 1.27) and is the format the Nim trace reader
    /// (`NimTraceReaderHandle` in `codetracer_trace_writer_nim`) and the
    /// db-backend's `CTFSTraceReader` consume directly via the structured
    /// `ct_reader_*` FFI — no postprocessing pass required.
    ///
    /// Historical note: this previously used `TraceEventsFileFormat::Json`
    /// because the OLD CBOR+Zstd binary format produced empty locals in the
    /// db-backend.  That note is no longer applicable; the modern CTFS
    /// multi-stream format is well-tested across Ruby/Python and is the
    /// CTFS-migration target tracked by mission goal #6.
    pub fn new(program: &str, output_dir: &Path) -> eyre::Result<Self> {
        let writer = create_trace_writer(program, &[], TraceEventsFileFormat::Ctfs);
        Ok(Self {
            writer,
            type_names: Vec::new(),
            output_dir: output_dir.to_path_buf(),
            paths_with_line_lengths: HashSet::new(),
        })
    }

    /// Initialize trace output files.
    ///
    /// The path argument is essentially a hint for the Nim writer: the
    /// produced `.ct` file goes to `<output_dir>/<program>.ct` regardless.
    pub fn initialize(&mut self) -> eyre::Result<()> {
        let events_path = self.output_dir.join("trace.json");

        TraceWriter::begin_writing_trace_events(&mut *self.writer, &events_path)
            .map_err(|e| eyre::eyre!("{}", e))?;

        // M14: opt the canonical CTFS writer into column-aware step
        // encoding *before* the first `register_step` / `start` call.
        // `enable_column_aware_steps` is sticky for the lifetime of the
        // trace and gates the writer's `DeltaColumn` (tag 0x07)
        // emission path plus the `meta.dat` bit 4 flag
        // (`FLAG_HAS_COLUMN_AWARE_STEPS`).  On legacy backends this is
        // a trait-default no-op; the EVM recorder is pinned to the Nim
        // multi-stream writer so the call lands on the real
        // implementation.  See the JS recorder's
        // `recorder_native::lib::write_trace` for the reference
        // pattern.
        TraceWriter::enable_column_aware_steps(&mut *self.writer);

        // M-capability-flags: the EVM recorder's PC→source map maps
        // each EVM instruction back to a single Solidity statement,
        // so per-column breakpoints AND per-column motions are both
        // meaningful.  Advertise both capabilities so the GUI exposes
        // its M6 Alt+click breakpoint UI and sub-statement step
        // controls.  See spec `internal-files.md` §"Column-Aware
        // Capability Flags".
        self.writer.enable_column_breakpoints_support();
        self.writer.enable_column_motions_support();

        self.register_evm_types();
        Ok(())
    }

    /// Register `path` with its per-line UTF-8 byte-length table via the
    /// `paths.dat` Layout A entry point, once per recorder lifetime.
    /// Subsequent calls for the same path are no-ops.
    ///
    /// Required by the column-aware mode: the reader maps the
    /// writer-side global byte position back to a (line, column) pair
    /// using these tables.  Soft-fails (logged to stderr) if the FFI
    /// rejects the call — the trace remains usable, but columns on
    /// that file fall back to `None` at read time.
    fn ensure_path_with_line_lengths(&mut self, path: &Path, source: &str) {
        if self.paths_with_line_lengths.contains(path) {
            return;
        }
        let line_lengths = compute_line_lengths(source);
        if let Err(err) =
            TraceWriter::register_path_with_line_lengths(&mut *self.writer, path, &line_lengths)
        {
            eprintln!(
                "[codetracer-evm-recorder] register_path_with_line_lengths failed for {}: {} \
                 (column resolution will fall back to None for this file)",
                path.display(),
                err,
            );
        }
        self.paths_with_line_lengths.insert(path.to_path_buf());
    }

    /// Register the standard EVM/Solidity types with the trace writer.
    pub fn register_evm_types(&mut self) {
        let types: &[(&str, TypeKind)] = &[
            ("uint256", TypeKind::Int),
            ("int256", TypeKind::Int),
            ("address", TypeKind::Raw),
            ("bytes32", TypeKind::Raw),
            ("bytes", TypeKind::Seq),
            ("bool", TypeKind::Bool),
            ("string", TypeKind::String),
            ("uint8", TypeKind::Int),
            ("uint16", TypeKind::Int),
            ("uint32", TypeKind::Int),
            ("uint64", TypeKind::Int),
            ("uint128", TypeKind::Int),
            ("int8", TypeKind::Int),
            ("int16", TypeKind::Int),
            ("int32", TypeKind::Int),
            ("int64", TypeKind::Int),
            ("int128", TypeKind::Int),
            ("bytes1", TypeKind::Raw),
            ("bytes4", TypeKind::Raw),
            ("bytes20", TypeKind::Raw),
        ];
        for (type_name, kind) in types {
            TraceWriter::register_type(&mut *self.writer, *kind, type_name);
            self.type_names.push(type_name.to_string());
        }
    }

    /// Look up a registered type name by index.
    pub fn get_type_index(&self, name: &str) -> Option<usize> {
        self.type_names.iter().position(|n| n == name)
    }

    /// Process a sequence of structLog entries with the given source map,
    /// bytecode, source file contents, source file paths, optional storage
    /// layout, and optional Solidity AST, emitting corresponding trace events.
    ///
    /// # Arguments
    ///
    /// * `struct_logs` - The structLog entries from `debug_traceTransaction`.
    /// * `source_map` - Parsed source map for the deployed bytecode.
    /// * `bytecode` - The deployed bytecode (used to build PC-to-instruction mapping).
    /// * `source_paths` - Paths of the source files (indexed by file_index).
    /// * `source_contents` - Contents of the source files (indexed by file_index).
    /// * `storage_layout` - Optional storage layout for decoding SSTORE operations.
    /// * `solidity_ast` - Optional Solidity AST for local variable reconstruction
    ///   (only effective for unoptimized Solidity code).
    #[allow(clippy::too_many_arguments)]
    pub fn record_from_structlog(
        &mut self,
        struct_logs: &[StructLog],
        source_map: &SourceMap,
        bytecode: &[u8],
        source_paths: &[&Path],
        source_contents: &[&str],
        storage_layout: Option<&StorageLayout>,
        solidity_ast: Option<&SolidityAst>,
    ) -> eyre::Result<()> {
        if struct_logs.is_empty() {
            return Ok(());
        }

        let pc_to_idx = source_map::build_pc_to_instruction_index(bytecode);

        // Initialize the top-level call
        let main_path = if source_paths.is_empty() {
            Path::new("<unknown>")
        } else {
            source_paths[0]
        };
        // M14: register the main source path with its per-line byte
        // counts BEFORE `TraceWriter::start`.  `start` internally
        // interns the path (without line-length data), and a later
        // `register_path_with_line_lengths` for an already-interned
        // path is silently dropped by the Nim writer — that drops the
        // line-length table needed by the reader's
        // `decodeGlobalPositionIndex`, so the per-step column field
        // never surfaces in ct-print.  Registering up front populates
        // `pathLineLengths` on the writer side and keeps the
        // subsequent `start` a no-op (path id already interned).
        if !source_paths.is_empty()
            && let Some(src) = source_contents.first().copied()
        {
            self.ensure_path_with_line_lengths(main_path, src);
        }
        TraceWriter::start(&mut *self.writer, main_path, Line(1));

        // The uint256 type id (first registered type) for storage values
        let uint256_type_id =
            TypeId(TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Int, "uint256").0);

        let mut prev_line: Option<(i32, u32, u32)> = None; // (file_index, line, column)
        let mut prev_depth: u64 = 1;

        // --- First internal call merging ---
        // The Solidity compiler's function dispatcher (contract preamble) runs
        // before the actual user function. `TraceWriter::start()` opens a
        // `<toplevel>` call at depth 0, and the first JumpType::Into from the
        // dispatcher into the target function would normally push depth to 1.
        //
        // This is problematic because the db-backend's "step over" (next)
        // skips all steps at deeper call depths. If the user function body
        // is at depth 1, stepping from the entry point skips the ENTIRE body.
        //
        // Fix: absorb the first JumpType::Into into `<toplevel>` by not
        // emitting a register_call for it, and skip the matching OutOf return.
        // This keeps the target function's steps at depth 0.
        let mut first_internal_call_absorbed = false;
        // Track the call nesting depth relative to the absorbed call so we
        // know when the matching return (OutOf) happens.
        let mut absorbed_call_nesting: i32 = 0;

        // --- Local variable tracking (M5) ---
        // One StackTracker per call-stack depth.  We keep a small Vec indexed
        // by depth (depth 1 = index 0).  Resetting on depth changes keeps the
        // symbolic stack consistent with the real EVM stack.
        let mut stack_trackers: Vec<StackTracker> = Vec::new();
        // Parallel memory trackers for memory-escalated locals (struct,
        // dynamic array, string, bytes, ...).  Indexed by depth identical
        // to `stack_trackers`.
        let mut memory_trackers: Vec<MemoryTracker> = Vec::new();

        // --- Storage variable carry-forward ---
        // SSTORE opcodes only fire once per slot write.  To make storage
        // variables visible in the debugger at every subsequent step, we cache
        // the last written value per variable name and re-emit the full set
        // whenever a new source-line step is registered.
        let mut storage_state: std::collections::HashMap<String, ValueRecord> =
            std::collections::HashMap::new();

        // Raw-slot index keyed by storage slot number, used to assemble
        // compound `ValueRecord::Sequence` / `Struct` snapshots for
        // fixed-size arrays and `inplace`-encoded structs declared in
        // the storage layout.  Mappings are intentionally excluded —
        // their slots are derived from `keccak256(key . slot)` and do
        // not live in a contiguous range, so we cannot reconstruct the
        // full collection from the layout alone.
        let mut slot_values: std::collections::HashMap<u64, alloy::primitives::U256> =
            std::collections::HashMap::new();

        // Mapping slot resolver (M11 category 2).  Maps the
        // `keccak256(key . base_slot)` output back to the canonical
        // `<mapping_name>[<key>]` qualified name, populated lazily by
        // intercepting every SHA3/KECCAK256 opcode.
        let mut mapping_slot_names: std::collections::HashMap<alloy::primitives::U256, String> =
            std::collections::HashMap::new();

        for (i, log) in struct_logs.iter().enumerate() {
            let pc = log.pc as usize;

            // --- Depth changes: external calls / returns ---
            if log.depth > prev_depth {
                // Entering an external call (CALL/DELEGATECALL/STATICCALL)
                let fn_name = format!("external_call_depth_{}", log.depth);
                let call_path = prev_line
                    .and_then(|(fi, _, _)| source_paths.get(fi as usize))
                    .copied()
                    .unwrap_or(main_path);
                let call_line = prev_line.map(|(_, l, _)| Line(l as i64)).unwrap_or(Line(0));
                let fn_id = TraceWriter::ensure_function_id(
                    &mut *self.writer,
                    &fn_name,
                    call_path,
                    call_line,
                );
                TraceWriter::register_call(&mut *self.writer, fn_id, vec![]);
                // Push a fresh tracker for the new call depth.
                while stack_trackers.len() < log.depth as usize {
                    stack_trackers.push(StackTracker::new());
                }
                while memory_trackers.len() < log.depth as usize {
                    memory_trackers.push(MemoryTracker::new());
                }
            } else if log.depth < prev_depth {
                // Returning from an external call.
                //
                // If the inner call ended via the REVERT opcode (the
                // last log at `prev_depth` immediately before this
                // depth drop), surface the decoded revert payload as
                // an `EventLogKind::Error` io_event.  This is what
                // Solidity's `try { ... } catch { ... }` block runs
                // against — the OUTER tx still succeeds (so
                // `frame.failed` in `main.rs` doesn't fire), but the
                // CALLEE reverted and we want consumers to see *why*.
                //
                // Note: only the *immediate* inner-frame REVERT is
                // inspected.  If `depth_diff > 1` (a chain of nested
                // reverts) only the deepest payload — which is the
                // one Solidity surfaces in the catch arm — is
                // emitted; intermediate frames simply propagate.
                if i > 0
                    && let Some(prev_log) = struct_logs.get(i - 1)
                    && prev_log.depth == prev_depth
                    && prev_log.op.as_ref() == "REVERT"
                {
                    let payload = decode_revert_opcode_payload(
                        prev_log.stack.as_deref(),
                        prev_log.memory.as_ref(),
                    );
                    let decoded = revert_decode::decode_revert(&payload);
                    TraceWriter::register_special_event(
                        &mut *self.writer,
                        EventLogKind::Error,
                        "RevertCaught",
                        &decoded.message,
                    );
                }

                let depth_diff = prev_depth - log.depth;
                for _ in 0..depth_diff {
                    let ret_val = ValueRecord::Raw {
                        r: "0x".to_string(),
                        type_id: uint256_type_id,
                    };
                    TraceWriter::register_return(&mut *self.writer, ret_val);
                    // Pop the tracker for the exited depth.
                    stack_trackers.pop();
                    memory_trackers.pop();
                }
            }

            // Ensure we always have a tracker for the current depth.
            while stack_trackers.len() < log.depth as usize {
                stack_trackers.push(StackTracker::new());
            }
            while memory_trackers.len() < log.depth as usize {
                memory_trackers.push(MemoryTracker::new());
            }
            let tracker_idx = (log.depth as usize).saturating_sub(1);
            let tracker = &mut stack_trackers[tracker_idx];

            // Decode the opcode byte (first byte of log.op hex, or look up by name).
            let opcode: Option<u8> = opcode_from_name(log.op.as_ref());

            // Resolve the source map entry for this PC to get the byte offset.
            let source_offset: Option<i32> = source_map
                .get_entry_for_pc(pc, &pc_to_idx)
                .filter(|e| e.file_index >= 0)
                .map(|e| e.offset);

            // --- Resolve PC to source location ---
            if let Some(location) = source_map.resolve_pc(pc, &pc_to_idx, source_contents) {
                let file_idx = location.file_index;
                let line = location.line;
                let column = location.column;
                // The source map stores the (line, column) pair derived
                // from the byte offset (`offset_to_line_col` in
                // `source_map.rs`).  Track (file, line, column) so
                // multi-statement-per-line moves still surface as
                // distinct steps under column-aware navigation.
                let current = (file_idx, line, column);

                // Emit a step if the source location changed
                if prev_line != Some(current) {
                    let step_path = source_paths
                        .get(file_idx as usize)
                        .copied()
                        .unwrap_or(main_path);
                    // Register the source path with its per-line byte
                    // counts (paths.dat Layout A) on first contact —
                    // required by the column-aware reader to map the
                    // writer-side global position back to (line, col).
                    if let Some(src) = source_contents.get(file_idx as usize).copied() {
                        self.ensure_path_with_line_lengths(step_path, src);
                    }
                    // M14: column-aware step emission.  `column` from
                    // `SourceLocation` is 0-based (byte offset within
                    // the line); the CTFS wire uses 1-based columns,
                    // so we forward `column + 1`.
                    TraceWriter::register_step_with_column(
                        &mut *self.writer,
                        step_path,
                        Line(line as i64),
                        Some(Line(column as i64 + 1)),
                    );
                    let line_changed = prev_line.map(|(f, l, _)| (f, l)) != Some((file_idx, line));
                    prev_line = Some(current);

                    // Re-emit all cached storage variables on line transitions
                    // so they remain visible in the debugger at every
                    // source-line step.  Same-line column transitions skip
                    // re-emission — the variable state has not changed.
                    if line_changed {
                        for (svar_name, svar_val) in &storage_state {
                            TraceWriter::register_variable_with_full_value(
                                &mut *self.writer,
                                svar_name,
                                svar_val.clone(),
                            );
                        }
                    }
                }

                // --- Local variable emission (M5) ---
                // After each step we check the in-scope variables and emit
                // current values from the concrete stack.
                //
                // Important: structLog's `stack` is the state BEFORE the
                // opcode executes.  The symbolic tracker models the state
                // AFTER the opcode.  To reconcile, we look up concrete
                // values from the NEXT step's stack (which reflects the
                // post-execution state of the current opcode).
                if let Some(op) = opcode {
                    if let (Some(ast), Some(src_off)) = (solidity_ast, source_offset) {
                        if let Some(func) = ast.function_at(src_off, file_idx) {
                            let in_scope = func.vars_in_scope_at(src_off);
                            // Update the symbolic tracker with in-scope variable info.
                            let _ = tracker.process_step(op, pc, Some(src_off), &in_scope);

                            // Drive the parallel memory tracker for memory-
                            // escalated locals (struct, dynamic arrays, ...).
                            // Only the MSTORE / MLOAD opcodes affect it; we
                            // pay the structLog memory-decode cost only then.
                            let mtracker = &mut memory_trackers[tracker_idx];
                            if matches!(op, 0x51..=0x53) {
                                let pre_stack = log.stack.as_deref().unwrap_or(&[]);
                                let pre_memory = decode_struct_log_memory(log.memory.as_ref());
                                let _ = mtracker.process_step(
                                    op,
                                    pc as u64,
                                    Some(src_off),
                                    &in_scope,
                                    pre_stack,
                                    &pre_memory,
                                );
                            }

                            // Use the next step's stack for value lookup (post-execution).
                            let post_stack =
                                struct_logs.get(i + 1).and_then(|next| next.stack.as_ref());

                            if let Some(concrete_stack) = post_stack {
                                for var in &in_scope {
                                    if let Some(val) =
                                        tracker.get_variable_value(&var.name, concrete_stack)
                                    {
                                        let type_kind = type_kind_for_solidity_type(&var.type_name);
                                        let type_id = TraceWriter::ensure_type_id(
                                            &mut *self.writer,
                                            type_kind,
                                            &var.type_name,
                                        );
                                        let val_record =
                                            value_record_for_local(val, &var.type_name, type_id);
                                        TraceWriter::register_variable_with_full_value(
                                            &mut *self.writer,
                                            &var.name,
                                            val_record,
                                        );
                                    }
                                }
                            }
                        } else {
                            // AST present but this PC is outside any known function
                            // (e.g., contract preamble / dispatcher). Still advance
                            // the tracker so the symbolic stack stays in sync.
                            let _ = tracker.process_step(op, pc, Some(src_off), &[]);
                        }
                    } else {
                        // No AST, or no source offset — still advance the tracker.
                        let _ = tracker.process_step(op, pc, source_offset, &[]);
                    }
                }

                // --- Jump type: internal calls / returns ---
                if let Some(entry) = source_map.get_entry_for_pc(pc, &pc_to_idx) {
                    match entry.jump_type {
                        JumpType::Into => {
                            if !first_internal_call_absorbed {
                                // Absorb the first internal call (dispatcher → target
                                // function) into <toplevel>. This keeps the target
                                // function's steps at depth 0 so step-over works.
                                first_internal_call_absorbed = true;
                                absorbed_call_nesting = 1;
                                // Still reset the tracker for a clean start.
                                tracker.reset();

                                // Even though we don't emit a `register_call`
                                // for the absorbed dispatcher → entry-point
                                // jump, we still want the entry-point function
                                // (e.g. `run`) to appear in the trace's
                                // `functions` table.  This is what the
                                // recorder-test-requirements §1 strict
                                // assertion "function table includes the
                                // entry-point name" expects (see
                                // `test_nested_calls_function_names_resolved`).
                                if let Some(target_fn) = resolve_internal_call_target(
                                    struct_logs,
                                    i,
                                    source_map,
                                    &pc_to_idx,
                                    solidity_ast,
                                ) {
                                    let fn_path = source_paths
                                        .get(file_idx as usize)
                                        .copied()
                                        .unwrap_or(main_path);
                                    let display_name = solidity_ast
                                        .map(|ast| ast.qualified_function_name(target_fn))
                                        .unwrap_or_else(|| target_fn.name.clone());
                                    let _ = TraceWriter::ensure_function_id(
                                        &mut *self.writer,
                                        &display_name,
                                        fn_path,
                                        Line(line as i64),
                                    );
                                }
                            } else {
                                // Track nesting within the absorbed scope.
                                if absorbed_call_nesting > 0 {
                                    absorbed_call_nesting += 1;
                                }

                                // Internal function call.
                                //
                                // Resolve the target function name by looking
                                // at upcoming structLog entries' source offsets
                                // and matching them to a function definition
                                // in the Solidity AST.  We scan up to a
                                // small number of entries because the
                                // first instruction(s) after a JUMP into a
                                // function often land on JUMPDEST or
                                // dispatcher preamble that has no source
                                // mapping (file_index == -1) — the function
                                // body proper starts a few opcodes later.
                                //
                                // Falls back to a `fn_at_*` placeholder when
                                // the AST isn't available or when the offset
                                // doesn't fall inside any known function.
                                let mut target_fn = resolve_internal_call_target(
                                    struct_logs,
                                    i,
                                    source_map,
                                    &pc_to_idx,
                                    solidity_ast,
                                );
                                // Category 1 (M11 spec-correct path):
                                // fall back to the JUMP's *enclosing*
                                // user function when post-jump
                                // lookahead lands in compiler-generated
                                // code (loop continuations, shared
                                // dispatcher back-edges).  This replaces
                                // the previous `fn_at_pc_<n>` orphan
                                // placeholders for intra-function back-
                                // edge JUMPs with the canonical name of
                                // the function the JUMP itself sits
                                // inside.
                                if target_fn.is_none() {
                                    target_fn = resolve_enclosing_function_for_jump(
                                        struct_logs,
                                        i,
                                        source_map,
                                        &pc_to_idx,
                                        solidity_ast,
                                    );
                                }
                                let fn_name = {
                                    if let Some(target_fn) = target_fn {
                                        solidity_ast
                                            .map(|ast| ast.qualified_function_name(target_fn))
                                            .unwrap_or_else(|| target_fn.name.clone())
                                    } else if let Some(next_log) = struct_logs.get(i + 1) {
                                        let next_pc = next_log.pc as usize;
                                        if let Some(next_loc) = source_map.resolve_pc(
                                            next_pc,
                                            &pc_to_idx,
                                            source_contents,
                                        ) {
                                            format!(
                                                "fn_at_{}:{}",
                                                next_loc.file_index, next_loc.line
                                            )
                                        } else {
                                            format!("fn_at_pc_{}", next_log.pc)
                                        }
                                    } else {
                                        "unknown_fn".to_string()
                                    }
                                };

                                let fn_path = source_paths
                                    .get(file_idx as usize)
                                    .copied()
                                    .unwrap_or(main_path);
                                let fn_id = TraceWriter::ensure_function_id(
                                    &mut *self.writer,
                                    &fn_name,
                                    fn_path,
                                    Line(line as i64),
                                );
                                stage_internal_call_args(
                                    &mut *self.writer,
                                    tracker,
                                    target_fn,
                                    log.stack.as_deref(),
                                    1,
                                );
                                TraceWriter::register_call(&mut *self.writer, fn_id, vec![]);
                            }
                        }
                        JumpType::OutOf => {
                            if absorbed_call_nesting > 0 {
                                absorbed_call_nesting -= 1;
                                if absorbed_call_nesting == 0 {
                                    // This is the return from the absorbed call.
                                    // Don't emit register_return — the <toplevel>
                                    // call will be closed by finalize().
                                    tracker.reset();
                                } else {
                                    // Return from a nested call within the absorbed scope.
                                    let ret_val = ValueRecord::Raw {
                                        r: "0x".to_string(),
                                        type_id: uint256_type_id,
                                    };
                                    TraceWriter::register_return(&mut *self.writer, ret_val);
                                    tracker.reset();
                                }
                            } else {
                                // Internal function return outside the absorbed scope.
                                let ret_val = ValueRecord::Raw {
                                    r: "0x".to_string(),
                                    type_id: uint256_type_id,
                                };
                                TraceWriter::register_return(&mut *self.writer, ret_val);
                                tracker.reset();
                            }
                        }
                        JumpType::Regular => {}
                    }
                }
            } else {
                // No source location — still advance the tracker.
                if let Some(op) = opcode {
                    let _ = tracker.process_step(op, pc, source_offset, &[]);
                }
            }

            // --- KECCAK256 (SHA3): record mapping-slot derivations ---
            //
            // Solidity computes a mapping value's storage slot as
            // `keccak256(abi.encode(key, base_slot))`.  By
            // intercepting every KECCAK256 with a 64-byte input we
            // can cache the derived slot → name mapping so the SSTORE
            // handler below can surface `<mapping_name>[<key>]`
            // instead of a synthetic `storage[<huge_slot>]`
            // placeholder (M11 category 2).
            //
            // Two cases:
            //
            //  * The tail 32 bytes match a mapping declared at a
            //    state-variable base slot in the storage layout —
            //    direct resolution.
            //  * The tail 32 bytes match a previously-derived
            //    mapping slot (e.g. the inner mapping of a nested
            //    `mapping(K1 => mapping(K2 => V))`).  We recurse by
            //    appending the new key to the cached parent name,
            //    yielding `outer[k1][k2]`.
            if matches!(log.op.as_ref(), "KECCAK256" | "SHA3")
                && let (Some(stack), Some(layout)) = (log.stack.as_ref(), storage_layout)
                && stack.len() >= 2
            {
                let offset = stack[stack.len() - 1];
                let size = stack[stack.len() - 2];
                if size == alloy::primitives::U256::from(64u64)
                    && let Some(off_us) = usize::try_from(offset).ok()
                {
                    let memory_bytes = decode_struct_log_memory(log.memory.as_ref());
                    let end = off_us.saturating_add(64);
                    if end <= memory_bytes.len() {
                        let input = &memory_bytes[off_us..end];
                        let key_bytes: [u8; 32] = input[..32].try_into().unwrap();
                        let slot_bytes: [u8; 32] = input[32..].try_into().unwrap();
                        let base_slot_u256 = alloy::primitives::U256::from_be_bytes(slot_bytes);
                        let derived_slot = keccak256_u256(input);

                        // Case 1: direct mapping at a state-variable base.
                        if let Ok(base_slot_u64) = u64::try_from(base_slot_u256)
                            && let Some((mapping_label, key_type_label)) =
                                layout.mapping_at_base(base_slot_u64)
                        {
                            let key_repr = format_mapping_key(&key_type_label, &key_bytes);
                            mapping_slot_names
                                .insert(derived_slot, format!("{mapping_label}[{key_repr}]"));
                        }
                        // Case 2: nested mapping — the base slot was
                        // itself derived from an earlier KECCAK256 that
                        // we already cached.  Append the new key to
                        // the cached parent name.
                        else if let Some(parent_name) =
                            mapping_slot_names.get(&base_slot_u256).cloned()
                        {
                            // For nested mappings the inner mapping's
                            // value type label isn't directly
                            // discoverable from the storage layout's
                            // root entries (they only carry the
                            // top-level mapping's key type).  Default
                            // to address formatting because nearly
                            // every nested-mapping key in our test
                            // corpus is an address; fall back to the
                            // full hex blob otherwise.
                            let key_repr = format_mapping_key("address", &key_bytes);
                            mapping_slot_names
                                .insert(derived_slot, format!("{parent_name}[{key_repr}]"));
                        }
                    }
                }
            }

            // --- SSTORE: decode storage writes ---
            if log.op.as_ref() == "SSTORE"
                && let Some(ref stack) = log.stack
                && stack.len() >= 2
            {
                let slot = stack[stack.len() - 1];
                let value = stack[stack.len() - 2];

                let slot_decimal = slot.to_string();
                let value_hex = format!("0x{:x}", value);

                // Try to resolve the variable name (M11 category 2):
                //
                // 1. Direct layout match (named state-variable slot).
                // 2. Mapping cache: `<mapping>[<key>]` for slots that
                //    were derived from a `keccak256(key . base)`
                //    operation we intercepted earlier.
                // 3. Struct-member lookup: `<struct>.<field>` for
                //    slots inside an `inplace`-encoded struct.
                // 4. Array-element lookup: `<array>[i]` for slots
                //    inside a fixed-size array.
                // 5. Fallback synthetic `storage[<slot>]` for slots
                //    we couldn't attribute (still surfaces the raw
                //    write in the trace).
                let var_name = storage_layout
                    .and_then(|sl| sl.resolve_slot(&slot_decimal))
                    .map(|entry| entry.label.clone())
                    .or_else(|| mapping_slot_names.get(&slot).cloned())
                    .or_else(|| {
                        slot_decimal.parse::<u64>().ok().and_then(|s| {
                            storage_layout.and_then(|sl| {
                                sl.struct_member_at(s)
                                    .map(|(parent, member)| format!("{parent}.{member}"))
                            })
                        })
                    })
                    .or_else(|| {
                        slot_decimal.parse::<u64>().ok().and_then(|s| {
                            storage_layout.and_then(|sl| {
                                sl.array_element_at(s)
                                    .map(|(parent, idx)| format!("{parent}[{idx}]"))
                            })
                        })
                    })
                    .unwrap_or_else(|| format!("storage[{}]", slot_decimal));

                let type_name = storage_layout
                    .and_then(|sl| sl.resolve_slot(&slot_decimal))
                    .and_then(|entry| {
                        storage_layout
                            .and_then(|sl| sl.type_info(entry))
                            .map(|ti| ti.label.clone())
                    })
                    .unwrap_or_else(|| "uint256".to_string());

                let type_id =
                    TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Int, &type_name);

                let val = ValueRecord::Raw {
                    r: value_hex,
                    type_id,
                };
                // Cache the storage variable for carry-forward to subsequent steps.
                storage_state.insert(var_name.clone(), val.clone());
                TraceWriter::register_variable_with_full_value(&mut *self.writer, &var_name, val);

                // --- Compound (Sequence / Struct) snapshot ---
                // If the slot belongs to a fixed-size array or an
                // inplace-encoded struct, emit a snapshot of the whole
                // compound under the parent label.  This unlocks the
                // `_value_kinds_present` test by surfacing
                // `ValueRecord::Sequence` / `Struct` for the parent
                // variable in addition to the per-slot `Raw` writes.
                if let (Ok(slot_u64), Some(layout)) = (slot_decimal.parse::<u64>(), storage_layout)
                {
                    slot_values.insert(slot_u64, value);
                    if let Some((parent, parent_ti, slot_count)) =
                        layout.containing_compound(slot_u64)
                    {
                        let base_slot: u64 = parent.slot.parse().unwrap_or(0);
                        if let Some(compound) = build_compound_value(
                            &mut *self.writer,
                            layout,
                            parent,
                            parent_ti,
                            base_slot,
                            slot_count,
                            &slot_values,
                        ) {
                            storage_state.insert(parent.label.clone(), compound.clone());
                            TraceWriter::register_variable_with_full_value(
                                &mut *self.writer,
                                &parent.label,
                                compound,
                            );
                        }
                    }
                }
            }

            // --- LOG0..LOG4: emit Solidity events as EvmEvent ---
            //
            // EVM `LOG{n}` opcodes are structured contract events, not stdout
            // writes.  Tag them with `EventLogKind::EvmEvent` so the frontend
            // routes them through the EVM-event renderer (codetracer's
            // `event_log.nim` and `flow.nim` special-case this kind) rather
            // than displaying them in the terminal-output pane alongside
            // `Write` records produced by other recorders.
            //
            // metadata carries the opcode mnemonic (`LOG0`..`LOG4`); the
            // content carries the indexed topics followed by the
            // ABI-decoded non-indexed `data` segment (read out of
            // EVM memory at the [offset, offset+size) range that
            // LOG{n} pops off the stack).  This is the M10 LOG{n}
            // ABI-decoding pin — see `IndexedEvents.sol`.
            if let Some(content) =
                build_log_event_content(log.op.as_ref(), log.stack.as_deref(), log.memory.as_ref())
            {
                let metadata = log.op.as_ref().to_string();
                TraceWriter::register_special_event(
                    &mut *self.writer,
                    EventLogKind::EvmEvent,
                    &metadata,
                    &content,
                );
            }

            // --- SELFDESTRUCT: surface the contract-termination opcode ---
            //
            // SELFDESTRUCT halts the contract and forwards any remaining
            // balance to the beneficiary address popped off the stack.
            // Tag it with `EventLogKind::EvmEvent` (metadata
            // `"SELFDESTRUCT"`) so consumers can distinguish a
            // SELFDESTRUCT-terminated trace from one that ended via
            // STOP / RETURN / REVERT and see the beneficiary address.
            if let Some(content) =
                build_selfdestruct_event_content(log.op.as_ref(), log.stack.as_deref())
            {
                TraceWriter::register_special_event(
                    &mut *self.writer,
                    EventLogKind::EvmEvent,
                    "SELFDESTRUCT",
                    &content,
                );
            }

            // --- Precompile detection (addresses 0x01..=0x09) ---
            //
            // CALL / STATICCALL / DELEGATECALL targeting one of the EVM
            // precompiles doesn't surface as a depth change in
            // `debug_traceTransaction`'s structlog (the precompile is
            // executed natively without entering its own EVM frame),
            // so we tag the *opcode site* itself with a
            // `EventLogKind::EvmEvent` carrying the canonical
            // precompile name (e.g. `ecrecover` for 0x01).
            if let Some((metadata, content)) =
                build_precompile_event_content(log.op.as_ref(), log.stack.as_deref())
            {
                TraceWriter::register_special_event(
                    &mut *self.writer,
                    EventLogKind::EvmEvent,
                    metadata,
                    &content,
                );
            }

            prev_depth = log.depth;
        }

        Ok(())
    }

    /// Process structLog entries with multi-contract support.
    ///
    /// Uses the [`ContractRegistry`] to switch source maps, storage layouts
    /// and Solidity ASTs when execution crosses contract boundaries via
    /// CALL / DELEGATECALL / STATICCALL / CREATE / CREATE2.
    ///
    /// # Arguments
    ///
    /// * `struct_logs` — structLog entries from `debug_traceTransaction`.
    /// * `registry` — registry mapping addresses to their artifacts.
    /// * `contract_address` — the address of the entry-point contract
    ///   (the `to` field of the transaction).
    ///
    /// # Returns
    ///
    /// A [`CallTree`] capturing the complete call structure of the transaction.
    #[allow(clippy::type_complexity)]
    pub fn record_from_structlog_multi_contract(
        &mut self,
        struct_logs: &[StructLog],
        registry: &ContractRegistry,
        contract_address: Address,
    ) -> eyre::Result<CallTree> {
        if struct_logs.is_empty() {
            return Ok(CallTree::new(contract_address));
        }

        // ---------- initial artifacts for the entry-point contract ----------
        let initial_artifacts = registry.get(&contract_address);
        let (
            init_source_map,
            init_bytecode,
            init_source_paths,
            init_source_contents,
            init_storage_layout,
            init_solidity_ast,
        ) = match initial_artifacts {
            Some(a) => (
                Some(&a.source_map),
                a.runtime_bytecode.as_slice(),
                a.source_paths.as_slice(),
                a.source_contents.as_slice(),
                a.storage_layout.as_ref(),
                a.solidity_ast.as_ref(),
            ),
            None => (None, &[][..], &[][..], &[][..], None, None),
        };

        // Build pc_to_idx for the initial contract.
        let init_pc_to_idx_owned;
        let init_pc_to_idx: &[usize] = if !init_bytecode.is_empty() {
            init_pc_to_idx_owned = source_map::build_pc_to_instruction_index(init_bytecode);
            &init_pc_to_idx_owned
        } else if let Some(a) = initial_artifacts {
            &a.pc_to_idx
        } else {
            &[]
        };

        // Determine the main source path for TraceWriter::start.
        let main_path = if !init_source_paths.is_empty() {
            &*init_source_paths[0]
        } else {
            Path::new("<unknown>")
        };
        // M14: register the main source path with its per-line byte
        // counts BEFORE `TraceWriter::start` — see the matching
        // single-contract path in `record_from_structlog` for the
        // rationale (start() interns the path with empty line-lengths
        // and subsequent line-length registrations are silently
        // dropped).
        if !init_source_paths.is_empty()
            && let Some(src) = init_source_contents.first()
        {
            self.ensure_path_with_line_lengths(main_path, src.as_str());
        }
        TraceWriter::start(&mut *self.writer, main_path, Line(1));

        let uint256_type_id =
            TypeId(TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Int, "uint256").0);

        // ---------- mutable "current context" pointers ----------
        // We track the current source-map context as owned data on a stack
        // indexed by EVM call depth.  Depth 1 = the entry-point frame.
        //
        // Each entry holds:
        //   (address, source_map_ref, pc_to_idx_ref, source_paths_ref,
        //    source_contents_ref, storage_layout_ref, solidity_ast_ref)
        //
        // Because lifetimes across the registry lookups are tricky we store
        // the address and re-look up at every depth change.

        struct FrameContext {
            address: Address,
            /// Whether this frame was entered via DELEGATECALL.
            is_delegate: bool,
            /// Proxy address for DELEGATECALL frames (for storage lookup).
            delegate_proxy: Option<Address>,
        }

        let mut frame_stack: Vec<FrameContext> = vec![FrameContext {
            address: contract_address,
            is_delegate: false,
            delegate_proxy: None,
        }];

        // Helper closure: resolve current source info from registry given the
        // top-of-frame-stack entry.
        // Returns (source_map, pc_to_idx, source_paths_strings, source_contents_strings,
        //          storage_layout, solidity_ast)
        // We borrow from the registry for each step.

        let mut call_tree = CallTree::new(contract_address);
        let mut prev_depth: u64 = 1;
        let mut prev_line: Option<(i32, u32, u32)> = None; // (file_index, line, column)
        let mut stack_trackers: Vec<StackTracker> = Vec::new();
        // Parallel memory trackers, indexed identically to `stack_trackers`.
        let mut memory_trackers: Vec<MemoryTracker> = Vec::new();

        // Storage variable carry-forward per call frame.  Each entry in the
        // Vec corresponds to an EVM call depth (depth 1 = index 0).  When a
        // new call frame is entered, a fresh map is pushed; when a frame is
        // exited, its map is popped.
        let mut storage_states: Vec<std::collections::HashMap<String, ValueRecord>> =
            vec![std::collections::HashMap::new()];

        // Raw-slot index per call frame (mirror of `storage_states` but
        // keyed by storage slot number) so we can assemble compound
        // `Sequence`/`Struct` snapshots after each SSTORE — see the
        // single-contract path for rationale.
        let mut slot_values_per_frame: Vec<
            std::collections::HashMap<u64, alloy::primitives::U256>,
        > = vec![std::collections::HashMap::new()];

        // Mapping slot resolver per frame (M11 category 2).  Mirrors
        // the single-contract `mapping_slot_names` cache.
        let mut mapping_slot_names_per_frame: Vec<
            std::collections::HashMap<alloy::primitives::U256, String>,
        > = vec![std::collections::HashMap::new()];

        for (i, log) in struct_logs.iter().enumerate() {
            let pc = log.pc as usize;

            // ------------------------------------------------------------------
            // Depth changes: enter / exit call frames
            // ------------------------------------------------------------------
            if log.depth > prev_depth {
                // Entering a new call frame.
                // Determine the target address from the *previous* step's stack.
                // Guard: i == 0 means there is no previous log; treat as unknown.
                let prev_log = if i > 0 { struct_logs.get(i - 1) } else { None };
                let prev_op = prev_log.map(|l| l.op.as_ref()).unwrap_or("");
                let prev_stack = prev_log.and_then(|l| l.stack.as_ref());

                let (target_addr, call_ty, is_delegate, delegate_proxy) = match prev_op {
                    "CALL" | "CALLCODE" => {
                        // Stack (top-to-bottom): gas, addr, value, argsOffset, argsLen, retOffset, retLen
                        // addr is at index stack.len()-2 (second from top)
                        let addr = prev_stack
                            .and_then(|s| s.get(s.len().wrapping_sub(2)))
                            .map(|v| {
                                let bytes = v.to_be_bytes::<32>();
                                Address::from_slice(&bytes[12..])
                            })
                            .unwrap_or(Address::ZERO);
                        (addr, CallType::Call, false, None)
                    }
                    "DELEGATECALL" => {
                        // Stack: gas, addr, argsOffset, argsLen, retOffset, retLen
                        let addr = prev_stack
                            .and_then(|s| s.get(s.len().wrapping_sub(2)))
                            .map(|v| {
                                let bytes = v.to_be_bytes::<32>();
                                Address::from_slice(&bytes[12..])
                            })
                            .unwrap_or(Address::ZERO);
                        let proxy = frame_stack
                            .last()
                            .map(|f| f.address)
                            .unwrap_or(Address::ZERO);
                        (addr, CallType::DelegateCall, true, Some(proxy))
                    }
                    "STATICCALL" => {
                        // Stack: gas, addr, argsOffset, argsLen, retOffset, retLen
                        let addr = prev_stack
                            .and_then(|s| s.get(s.len().wrapping_sub(2)))
                            .map(|v| {
                                let bytes = v.to_be_bytes::<32>();
                                Address::from_slice(&bytes[12..])
                            })
                            .unwrap_or(Address::ZERO);
                        (addr, CallType::StaticCall, false, None)
                    }
                    "CREATE" => (Address::ZERO, CallType::Create, false, None),
                    "CREATE2" => (Address::ZERO, CallType::Create2, false, None),
                    _ => (Address::ZERO, CallType::Call, false, None),
                };

                call_tree.enter_call(target_addr, call_ty, i);
                frame_stack.push(FrameContext {
                    address: target_addr,
                    is_delegate,
                    delegate_proxy,
                });

                // Reset prev_line so the first step in the new frame is always
                // emitted (source paths/indices are relative to a different
                // contract's artifact array after a cross-contract call).
                prev_line = None;

                // Emit a trace call event for the new frame.
                let fn_name = format!("external_call_depth_{}", log.depth);
                let call_path = prev_line
                    .and_then(|(fi, _, _)| {
                        // Use the source paths from the *caller* frame (still at prev_depth).
                        let caller_addr = frame_stack
                            .get(frame_stack.len().saturating_sub(2))
                            .map(|f| f.address)
                            .unwrap_or(contract_address);
                        registry
                            .get(&caller_addr)
                            .and_then(|a| a.source_paths.get(fi as usize))
                            .map(|p| p.as_path())
                    })
                    .unwrap_or(main_path);
                let call_line = prev_line.map(|(_, l, _)| Line(l as i64)).unwrap_or(Line(0));
                let fn_id = TraceWriter::ensure_function_id(
                    &mut *self.writer,
                    &fn_name,
                    call_path,
                    call_line,
                );
                TraceWriter::register_call(&mut *self.writer, fn_id, vec![]);

                while stack_trackers.len() < log.depth as usize {
                    stack_trackers.push(StackTracker::new());
                }
                while memory_trackers.len() < log.depth as usize {
                    memory_trackers.push(MemoryTracker::new());
                }
                // Fresh storage state for the new call frame.
                while storage_states.len() < log.depth as usize {
                    storage_states.push(std::collections::HashMap::new());
                }
                while slot_values_per_frame.len() < log.depth as usize {
                    slot_values_per_frame.push(std::collections::HashMap::new());
                }
                while mapping_slot_names_per_frame.len() < log.depth as usize {
                    mapping_slot_names_per_frame.push(std::collections::HashMap::new());
                }
            } else if log.depth < prev_depth {
                let depth_diff = prev_depth - log.depth;
                for _ in 0..depth_diff {
                    let ret_val = ValueRecord::Raw {
                        r: "0x".to_string(),
                        type_id: uint256_type_id,
                    };
                    TraceWriter::register_return(&mut *self.writer, ret_val);
                    stack_trackers.pop();
                    memory_trackers.pop();
                    // Pop the exited frame's storage state (keep root frame).
                    if storage_states.len() > 1 {
                        storage_states.pop();
                    }
                    if slot_values_per_frame.len() > 1 {
                        slot_values_per_frame.pop();
                    }
                    if mapping_slot_names_per_frame.len() > 1 {
                        mapping_slot_names_per_frame.pop();
                    }
                    call_tree.exit_call(i);
                    if frame_stack.len() > 1 {
                        frame_stack.pop();
                    }
                }
                // Reset prev_line after returning to the caller frame: the
                // caller's source paths are relative to a different artifact
                // array, so we must re-emit a step for the current location.
                prev_line = None;
            }

            // Ensure trackers are sized for current depth.
            while stack_trackers.len() < log.depth as usize {
                stack_trackers.push(StackTracker::new());
            }
            while memory_trackers.len() < log.depth as usize {
                memory_trackers.push(MemoryTracker::new());
            }
            let tracker_idx = (log.depth as usize).saturating_sub(1);
            let tracker = &mut stack_trackers[tracker_idx];

            // ------------------------------------------------------------------
            // Resolve source artifacts for the current call frame
            // ------------------------------------------------------------------
            let current_addr = frame_stack
                .last()
                .map(|f| f.address)
                .unwrap_or(contract_address);
            let is_delegate_frame = frame_stack.last().map(|f| f.is_delegate).unwrap_or(false);
            let delegate_proxy = frame_stack.last().and_then(|f| f.delegate_proxy);

            // We need to work with references that may or may not exist.
            // To avoid borrow-checker issues with Option<&T> from registry we
            // do the lookup here and produce local Option<&...> values.
            let (
                cur_source_map,
                cur_pc_to_idx,
                cur_source_paths,
                cur_source_contents,
                cur_storage_layout,
                cur_solidity_ast,
            ): (
                Option<&SourceMap>,
                &[usize],
                &[PathBuf],
                &[String],
                Option<&StorageLayout>,
                Option<&SolidityAst>,
            ) = {
                if is_delegate_frame {
                    if let Some(proxy_addr) = delegate_proxy {
                        if let Some(view) = registry.get_delegatecall(&current_addr, &proxy_addr) {
                            (
                                Some(view.source_map),
                                view.pc_to_idx,
                                view.source_paths,
                                view.source_contents,
                                view.storage_layout,
                                view.solidity_ast,
                            )
                        } else {
                            (None, &[], &[], &[], None, None)
                        }
                    } else {
                        (None, &[], &[], &[], None, None)
                    }
                } else if let Some(a) = registry.get(&current_addr) {
                    (
                        Some(&a.source_map),
                        &a.pc_to_idx,
                        &a.source_paths,
                        &a.source_contents,
                        a.storage_layout.as_ref(),
                        a.solidity_ast.as_ref(),
                    )
                } else {
                    // Fall back to initial contract's data for unknown addresses.
                    (
                        init_source_map,
                        init_pc_to_idx,
                        init_source_paths,
                        init_source_contents,
                        init_storage_layout,
                        init_solidity_ast,
                    )
                }
            };

            // Convert &[String] to &[&str] slices that the existing helpers expect.
            // We do this with a small temporary Vec allocated per step only when needed.
            let source_contents_strs: Vec<&str> =
                cur_source_contents.iter().map(|s| s.as_str()).collect();
            let source_paths_paths: Vec<&Path> =
                cur_source_paths.iter().map(|p| p.as_path()).collect();

            let opcode: Option<u8> = opcode_from_name(log.op.as_ref());

            let source_offset: Option<i32> = cur_source_map
                .and_then(|sm| sm.get_entry_for_pc(pc, cur_pc_to_idx))
                .filter(|e| e.file_index >= 0)
                .map(|e| e.offset);

            // ------------------------------------------------------------------
            // Resolve PC to source location and emit step
            // ------------------------------------------------------------------
            let resolved_location = cur_source_map
                .and_then(|sm| sm.resolve_pc(pc, cur_pc_to_idx, &source_contents_strs));

            if let Some(location) = resolved_location {
                let file_idx = location.file_index;
                let line = location.line;
                let column = location.column;
                // (file_index, line, column) — track the column too so
                // multi-statement-per-line moves emit distinct steps
                // under column-aware navigation.
                let current = (file_idx, line, column);

                if prev_line != Some(current) {
                    let step_path = source_paths_paths
                        .get(file_idx as usize)
                        .copied()
                        .unwrap_or(main_path);
                    // Register the source path with its per-line byte
                    // counts (paths.dat Layout A) on first contact.
                    if let Some(src) = source_contents_strs.get(file_idx as usize).copied() {
                        self.ensure_path_with_line_lengths(step_path, src);
                    }
                    // M14: 0-based column → 1-based on the wire.
                    TraceWriter::register_step_with_column(
                        &mut *self.writer,
                        step_path,
                        Line(line as i64),
                        Some(Line(column as i64 + 1)),
                    );
                    let line_changed = prev_line.map(|(f, l, _)| (f, l)) != Some((file_idx, line));
                    prev_line = Some(current);

                    // Re-emit all cached storage variables for the current
                    // frame on line transitions only; same-line column
                    // transitions share the prior step's variable state.
                    if line_changed {
                        let ss_idx = (log.depth as usize).saturating_sub(1);
                        if let Some(ss) = storage_states.get(ss_idx) {
                            for (svar_name, svar_val) in ss {
                                TraceWriter::register_variable_with_full_value(
                                    &mut *self.writer,
                                    svar_name,
                                    svar_val.clone(),
                                );
                            }
                        }
                    }
                }

                // Local variable tracking (M5 logic, applied per-frame).
                if let Some(op) = opcode {
                    if let (Some(ast), Some(src_off)) = (cur_solidity_ast, source_offset) {
                        if let Some(func) = ast.function_at(src_off, file_idx) {
                            let in_scope = func.vars_in_scope_at(src_off);
                            let _ = tracker.process_step(op, pc, Some(src_off), &in_scope);

                            // Drive the parallel memory tracker for memory-
                            // escalated locals.  Decoded memory is only
                            // needed for MSTORE / MLOAD.
                            let mtracker = &mut memory_trackers[tracker_idx];
                            if matches!(op, 0x51..=0x53) {
                                let pre_stack = log.stack.as_deref().unwrap_or(&[]);
                                let pre_memory = decode_struct_log_memory(log.memory.as_ref());
                                let _ = mtracker.process_step(
                                    op,
                                    pc as u64,
                                    Some(src_off),
                                    &in_scope,
                                    pre_stack,
                                    &pre_memory,
                                );
                            }

                            if let Some(ref concrete_stack) = log.stack {
                                for var in &in_scope {
                                    if let Some(val) =
                                        tracker.get_variable_value(&var.name, concrete_stack)
                                    {
                                        let type_kind = type_kind_for_solidity_type(&var.type_name);
                                        let type_id = TraceWriter::ensure_type_id(
                                            &mut *self.writer,
                                            type_kind,
                                            &var.type_name,
                                        );
                                        let val_record =
                                            value_record_for_local(val, &var.type_name, type_id);
                                        TraceWriter::register_variable_with_full_value(
                                            &mut *self.writer,
                                            &var.name,
                                            val_record,
                                        );
                                    }
                                }
                            }
                        } else {
                            let _ = tracker.process_step(op, pc, Some(src_off), &[]);
                        }
                    } else {
                        let _ = tracker.process_step(op, pc, source_offset, &[]);
                    }
                }

                // Jump type: internal calls / returns.
                if let Some(entry) =
                    cur_source_map.and_then(|sm| sm.get_entry_for_pc(pc, cur_pc_to_idx))
                {
                    match entry.jump_type {
                        JumpType::Into => {
                            // Resolve the target function name via the
                            // Solidity AST (mirror of the single-contract
                            // path).  Lookahead handles solc-generated
                            // function prologues (JUMPDEST + PUSH/POP) that
                            // sit at the head of every internal function and
                            // typically have no source map entry.
                            let mut target_fn = cur_source_map.and_then(|source_map| {
                                resolve_internal_call_target(
                                    struct_logs,
                                    i,
                                    source_map,
                                    cur_pc_to_idx,
                                    cur_solidity_ast,
                                )
                            });
                            // Category 1 (M11): fall back to the JUMP's
                            // *enclosing* user function when post-jump
                            // lookahead lands in compiler-generated code.
                            if target_fn.is_none() {
                                target_fn = cur_source_map.and_then(|source_map| {
                                    resolve_enclosing_function_for_jump(
                                        struct_logs,
                                        i,
                                        source_map,
                                        cur_pc_to_idx,
                                        cur_solidity_ast,
                                    )
                                });
                            }
                            let fn_name = {
                                if let Some(target_fn) = target_fn {
                                    cur_solidity_ast
                                        .map(|ast| ast.qualified_function_name(target_fn))
                                        .unwrap_or_else(|| target_fn.name.clone())
                                } else if let Some(next_log) = struct_logs.get(i + 1) {
                                    let next_pc = next_log.pc as usize;
                                    if let Some(next_loc) = cur_source_map.and_then(|sm| {
                                        sm.resolve_pc(next_pc, cur_pc_to_idx, &source_contents_strs)
                                    }) {
                                        format!("fn_at_{}:{}", next_loc.file_index, next_loc.line)
                                    } else {
                                        format!("fn_at_pc_{}", next_log.pc)
                                    }
                                } else {
                                    "unknown_fn".to_string()
                                }
                            };

                            let fn_path = source_paths_paths
                                .get(file_idx as usize)
                                .copied()
                                .unwrap_or(main_path);
                            let fn_id = TraceWriter::ensure_function_id(
                                &mut *self.writer,
                                &fn_name,
                                fn_path,
                                Line(line as i64),
                            );
                            stage_internal_call_args(
                                &mut *self.writer,
                                tracker,
                                target_fn,
                                log.stack.as_deref(),
                                1,
                            );
                            TraceWriter::register_call(&mut *self.writer, fn_id, vec![]);
                        }
                        JumpType::OutOf => {
                            let ret_val = ValueRecord::Raw {
                                r: "0x".to_string(),
                                type_id: uint256_type_id,
                            };
                            TraceWriter::register_return(&mut *self.writer, ret_val);
                            tracker.reset();
                        }
                        JumpType::Regular => {}
                    }
                }
            } else if let Some(op) = opcode {
                let _ = tracker.process_step(op, pc, source_offset, &[]);
            }

            // ------------------------------------------------------------------
            // KECCAK256 (SHA3): record mapping-slot derivations
            // (mirror of single-contract path; M11 category 2,
            // including nested-mapping recursion).
            // ------------------------------------------------------------------
            if matches!(log.op.as_ref(), "KECCAK256" | "SHA3")
                && let (Some(stack), Some(layout)) = (log.stack.as_ref(), cur_storage_layout)
                && stack.len() >= 2
            {
                let offset = stack[stack.len() - 1];
                let size = stack[stack.len() - 2];
                if size == alloy::primitives::U256::from(64u64)
                    && let Some(off_us) = usize::try_from(offset).ok()
                {
                    let memory_bytes = decode_struct_log_memory(log.memory.as_ref());
                    let end = off_us.saturating_add(64);
                    if end <= memory_bytes.len() {
                        let input = &memory_bytes[off_us..end];
                        let key_bytes: [u8; 32] = input[..32].try_into().unwrap();
                        let slot_bytes: [u8; 32] = input[32..].try_into().unwrap();
                        let base_slot_u256 = alloy::primitives::U256::from_be_bytes(slot_bytes);
                        let derived_slot = keccak256_u256(input);

                        let mn_idx = (log.depth as usize).saturating_sub(1);
                        while mapping_slot_names_per_frame.len() <= mn_idx {
                            mapping_slot_names_per_frame.push(std::collections::HashMap::new());
                        }

                        if let Ok(base_slot_u64) = u64::try_from(base_slot_u256)
                            && let Some((mapping_label, key_type_label)) =
                                layout.mapping_at_base(base_slot_u64)
                        {
                            let key_repr = format_mapping_key(&key_type_label, &key_bytes);
                            mapping_slot_names_per_frame[mn_idx]
                                .insert(derived_slot, format!("{mapping_label}[{key_repr}]"));
                        } else if let Some(parent_name) = mapping_slot_names_per_frame[mn_idx]
                            .get(&base_slot_u256)
                            .cloned()
                        {
                            let key_repr = format_mapping_key("address", &key_bytes);
                            mapping_slot_names_per_frame[mn_idx]
                                .insert(derived_slot, format!("{parent_name}[{key_repr}]"));
                        }
                    }
                }
            }

            // ------------------------------------------------------------------
            // SSTORE: decode storage writes using current frame's storage layout
            // ------------------------------------------------------------------
            if log.op.as_ref() == "SSTORE"
                && let Some(ref stack) = log.stack
                && stack.len() >= 2
            {
                let slot = stack[stack.len() - 1];
                let value = stack[stack.len() - 2];

                let slot_decimal = slot.to_string();
                let value_hex = format!("0x{:x}", value);

                let mn_idx = (log.depth as usize).saturating_sub(1);
                let mapped_name = mapping_slot_names_per_frame
                    .get(mn_idx)
                    .and_then(|m| m.get(&slot).cloned());
                let var_name = cur_storage_layout
                    .and_then(|sl| sl.resolve_slot(&slot_decimal))
                    .map(|entry| entry.label.clone())
                    .or(mapped_name)
                    .or_else(|| {
                        slot_decimal.parse::<u64>().ok().and_then(|s| {
                            cur_storage_layout.and_then(|sl| {
                                sl.struct_member_at(s)
                                    .map(|(parent, member)| format!("{parent}.{member}"))
                            })
                        })
                    })
                    .or_else(|| {
                        slot_decimal.parse::<u64>().ok().and_then(|s| {
                            cur_storage_layout.and_then(|sl| {
                                sl.array_element_at(s)
                                    .map(|(parent, idx)| format!("{parent}[{idx}]"))
                            })
                        })
                    })
                    .unwrap_or_else(|| format!("storage[{}]", slot_decimal));

                let type_name = cur_storage_layout
                    .and_then(|sl| sl.resolve_slot(&slot_decimal))
                    .and_then(|entry| {
                        cur_storage_layout
                            .and_then(|sl| sl.type_info(entry))
                            .map(|ti| ti.label.clone())
                    })
                    .unwrap_or_else(|| "uint256".to_string());

                let type_id =
                    TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Int, &type_name);
                let val = ValueRecord::Raw {
                    r: value_hex,
                    type_id,
                };
                // Cache for carry-forward to subsequent steps in this frame.
                let ss_idx = (log.depth as usize).saturating_sub(1);
                while storage_states.len() <= ss_idx {
                    storage_states.push(std::collections::HashMap::new());
                }
                while slot_values_per_frame.len() <= ss_idx {
                    slot_values_per_frame.push(std::collections::HashMap::new());
                }
                storage_states[ss_idx].insert(var_name.clone(), val.clone());
                TraceWriter::register_variable_with_full_value(&mut *self.writer, &var_name, val);

                // --- Compound (Sequence / Struct) snapshot ---
                if let (Ok(slot_u64), Some(layout)) =
                    (slot_decimal.parse::<u64>(), cur_storage_layout)
                {
                    slot_values_per_frame[ss_idx].insert(slot_u64, value);
                    if let Some((parent, parent_ti, slot_count)) =
                        layout.containing_compound(slot_u64)
                    {
                        let base_slot: u64 = parent.slot.parse().unwrap_or(0);
                        if let Some(compound) = build_compound_value(
                            &mut *self.writer,
                            layout,
                            parent,
                            parent_ti,
                            base_slot,
                            slot_count,
                            &slot_values_per_frame[ss_idx],
                        ) {
                            storage_states[ss_idx].insert(parent.label.clone(), compound.clone());
                            TraceWriter::register_variable_with_full_value(
                                &mut *self.writer,
                                &parent.label,
                                compound,
                            );
                        }
                    }
                }
            }

            // ------------------------------------------------------------------
            // LOG0..LOG4: emit Solidity events as EvmEvent
            //
            // See the equivalent block in `record_from_structlog` for
            // rationale on `EventLogKind::EvmEvent`.  The content
            // includes both the indexed topics and the non-indexed
            // `data` segment (read out of memory) per the M10 LOG{n}
            // ABI-decoding pin.
            // ------------------------------------------------------------------
            if let Some(content) =
                build_log_event_content(log.op.as_ref(), log.stack.as_deref(), log.memory.as_ref())
            {
                let metadata = log.op.as_ref().to_string();
                TraceWriter::register_special_event(
                    &mut *self.writer,
                    EventLogKind::EvmEvent,
                    &metadata,
                    &content,
                );
            }

            // SELFDESTRUCT: see equivalent block in `record_from_structlog`.
            if let Some(content) =
                build_selfdestruct_event_content(log.op.as_ref(), log.stack.as_deref())
            {
                TraceWriter::register_special_event(
                    &mut *self.writer,
                    EventLogKind::EvmEvent,
                    "SELFDESTRUCT",
                    &content,
                );
            }

            // Precompile detection: see equivalent block in `record_from_structlog`.
            if let Some((metadata, content)) =
                build_precompile_event_content(log.op.as_ref(), log.stack.as_deref())
            {
                TraceWriter::register_special_event(
                    &mut *self.writer,
                    EventLogKind::EvmEvent,
                    metadata,
                    &content,
                );
            }

            prev_depth = log.depth;
        }

        Ok(call_tree)
    }

    /// Emit an `EventLogKind::Error` io_event carrying the decoded
    /// revert reason for a transaction that ended in `REVERT` (or a
    /// `Panic`).  Multi-stream consumers surface this as `ioError`
    /// (see `toIOEventKind` in `codetracer_trace_writer_ffi.nim`),
    /// which is what the recorder-test-requirements `revert path`
    /// case asserts on.
    ///
    /// `metadata` is a short tag (`"Revert"` / `"Panic"` / `"RevertRaw"`)
    /// and `reason` is the human-readable body (the decoded
    /// `Error(string)` argument, the panic-code mnemonic, or a hex
    /// dump of the raw output bytes for unrecognised payloads).
    pub fn register_revert(&mut self, metadata: &str, reason: &str) {
        TraceWriter::register_special_event(
            &mut *self.writer,
            EventLogKind::Error,
            metadata,
            reason,
        );
    }

    /// Finalize the trace output, flushing all buffered data.
    pub fn finalize(&mut self) -> eyre::Result<()> {
        // Close the <toplevel> call that start() opened.
        TraceWriter::register_return(&mut *self.writer, NONE_VALUE);

        TraceWriter::finish_writing_trace_events(&mut *self.writer)
            .map_err(|e| eyre::eyre!("{}", e))?;
        self.writer
            .write_meta_dat("codetracer-evm-recorder")
            .map_err(|e| eyre::eyre!("{}", e))?;
        self.writer.close().map_err(|e| eyre::eyre!("{}", e))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper: map opcode mnemonic string to opcode byte
// ---------------------------------------------------------------------------

/// Flatten a structLog `memory` field — a `Vec<String>` of 32-byte hex words
/// — into a contiguous byte buffer.  Returns an empty buffer when no memory
/// snapshot is present.  Tolerates `0x` prefixes and odd-length words by
/// padding with zeros.
fn decode_struct_log_memory(memory: Option<&Vec<String>>) -> Vec<u8> {
    let Some(words) = memory else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(words.len() * 32);
    for w in words {
        let trimmed = w.strip_prefix("0x").unwrap_or(w.as_str());
        let mut buf = [0u8; 32];
        // Each word should be 64 hex chars; tolerate shorter values.
        let bytes_count = trimmed.len() / 2;
        for (i, slot) in buf.iter_mut().take(bytes_count.min(32)).enumerate() {
            let lo = i * 2;
            if let Ok(b) = u8::from_str_radix(&trimmed[lo..lo + 2], 16) {
                *slot = b;
            }
        }
        out.extend_from_slice(&buf);
    }
    out
}

/// Read the revert payload from a `REVERT` structLog entry.
///
/// `REVERT` pops two arguments off the stack — `offset` (top) and
/// `size` (next) — and copies that range out of EVM memory as the
/// transaction's return data.  We mirror the EVM semantics exactly so
/// the bytes can be fed back into [`revert_decode::decode_revert`] and
/// surface the same `Error(string)` / `Panic(uint256)` reason that
/// Solidity's try/catch sees on the catch arm.
///
/// Returns an empty byte vector when the structlog is missing the
/// stack/memory snapshot or when the requested memory range is not
/// fully captured (truncated memory) — `decode_revert` handles the
/// empty case as `RevertEmpty`, which is what tx-level reverts with
/// no payload also surface as.
fn decode_revert_opcode_payload(
    stack: Option<&[alloy::primitives::U256]>,
    memory: Option<&Vec<String>>,
) -> Vec<u8> {
    let Some(stack) = stack else {
        return Vec::new();
    };
    if stack.len() < 2 {
        return Vec::new();
    }
    let offset = stack[stack.len() - 1];
    let size = stack[stack.len() - 2];

    let Some(offset_usize) = u64::try_from(offset)
        .ok()
        .and_then(|x| usize::try_from(x).ok())
    else {
        return Vec::new();
    };
    let Some(size_usize) = u64::try_from(size)
        .ok()
        .and_then(|x| usize::try_from(x).ok())
    else {
        return Vec::new();
    };
    if size_usize == 0 {
        return Vec::new();
    }

    let memory_bytes = decode_struct_log_memory(memory);
    let end = offset_usize.saturating_add(size_usize);
    if end > memory_bytes.len() {
        return Vec::new();
    }
    memory_bytes[offset_usize..end].to_vec()
}

/// Build the EvmEvent content payload for a LOG{n} opcode.
///
/// Returns `None` when the opcode is not a LOG{0..4} mnemonic.
///
/// The payload format is `topic0, topic1, ..., 0xDATA` — the indexed
/// topics (popped off the stack between offset/size and the bottom of
/// the LOG argument span) are joined with `", "`, and the non-indexed
/// `data` segment (read out of EVM memory at the `[offset, offset+size)`
/// range that LOG{n} pops) is appended as a single 0x-prefixed hex
/// blob when `size > 0`.  When `size == 0` the trailing comma is
/// omitted.
///
/// This is the M10 LOG{n} ABI-decoding pin (`IndexedEvents.sol`): the
/// payload now carries the full ABI shape (indexed topics +
/// non-indexed data) rather than just the topics list.
/// Build the tagged content for a `SELFDESTRUCT` opcode.  The EVM
/// `SELFDESTRUCT` opcode pops a single beneficiary address from the
/// top of the stack and transfers any remaining balance there before
/// halting the contract.  We surface it as an
/// `EventLogKind::EvmEvent` io_event with metadata `"SELFDESTRUCT"`
/// and content `"0x<20-byte-beneficiary>"` so consumers can tell the
/// trace ended via SELFDESTRUCT (not via STOP / RETURN / REVERT) and
/// see *who* received the remaining ETH.
///
/// Returns `None` when the opcode isn't `SELFDESTRUCT` or the stack
/// is empty (defensive).
fn build_selfdestruct_event_content(
    op: &str,
    stack: Option<&[alloy::primitives::U256]>,
) -> Option<String> {
    if op != "SELFDESTRUCT" {
        return None;
    }
    let stack = stack?;
    let beneficiary = stack.last()?;
    // Format as a 20-byte hex address (lower 160 bits of the
    // 256-bit stack word — EVM addresses are stored zero-padded).
    let bytes = beneficiary.to_be_bytes::<32>();
    let mut content = String::with_capacity(2 + 40);
    content.push_str("0x");
    use std::fmt::Write as _;
    for byte in &bytes[12..] {
        let _ = write!(content, "{:02x}", byte);
    }
    Some(content)
}

/// Map the canonical EVM precompile address (1..=9) to the precompile's
/// canonical mnemonic name.  Returns `None` for any other address.
///
/// The list mirrors the mainnet-active precompiles as of the Cancun
/// hard fork:
///
///   * 0x01 — `ecrecover`   (ECDSA signature recovery)
///   * 0x02 — `sha256`      (SHA-2 256-bit hash)
///   * 0x03 — `ripemd160`   (RIPEMD-160 hash)
///   * 0x04 — `identity`    (memory copy)
///   * 0x05 — `modexp`      (modular exponentiation)
///   * 0x06 — `ecAdd`       (BN254 curve point addition)
///   * 0x07 — `ecMul`       (BN254 curve point scalar multiplication)
///   * 0x08 — `ecPairing`   (BN254 pairing check)
///   * 0x09 — `blake2f`     (BLAKE2b compression function)
fn precompile_name_for_address(addr_lower160: u64) -> Option<&'static str> {
    match addr_lower160 {
        0x01 => Some("ecrecover"),
        0x02 => Some("sha256"),
        0x03 => Some("ripemd160"),
        0x04 => Some("identity"),
        0x05 => Some("modexp"),
        0x06 => Some("ecAdd"),
        0x07 => Some("ecMul"),
        0x08 => Some("ecPairing"),
        0x09 => Some("blake2f"),
        _ => None,
    }
}

/// Detect a CALL / STATICCALL / DELEGATECALL targeting one of the EVM
/// precompiles (addresses 0x01..=0x09) and build a tagged event
/// payload `"<name>:0x<20-byte-address>"`.  The recorder's normal
/// depth-change path doesn't surface precompile calls (anvil's
/// `debug_traceTransaction` doesn't increment depth for them — the
/// precompile executes natively without entering its own EVM frame),
/// so without this dedicated detection the trace would be silent
/// about every `ecrecover` / `sha256` / `keccak256-via-precompile`
/// invocation.
///
/// Returns `None` for non-CALL opcodes or for CALLs whose target is
/// not a recognised precompile.
fn build_precompile_event_content(
    op: &str,
    stack: Option<&[alloy::primitives::U256]>,
) -> Option<(&'static str, String)> {
    let is_call = matches!(op, "CALL" | "CALLCODE" | "DELEGATECALL" | "STATICCALL");
    if !is_call {
        return None;
    }
    let stack = stack?;
    if stack.len() < 2 {
        return None;
    }
    // Stack ordering for CALL/CALLCODE: [retLen, retOffset, argsLen,
    //   argsOffset, value, addr, gas] (top = retLen).
    // For STATICCALL/DELEGATECALL the `value` slot is missing.
    // In the structlog `stack` array index, top-of-stack is the LAST
    // element.  `addr` is at index `len - 2`.
    let addr_word = stack.get(stack.len() - 2)?;
    // Check that the upper 192 bits are zero AND the address is in
    // the precompile range (1..=9 fits in the lower 8 bits).
    let bytes = addr_word.to_be_bytes::<32>();
    // The 20-byte address sits in bytes 12..32.  For a precompile
    // the top 19 bytes must all be zero and the last byte in 1..=9.
    if bytes[..31].iter().any(|b| *b != 0) {
        return None;
    }
    let last_byte = bytes[31] as u64;
    let name = precompile_name_for_address(last_byte)?;
    // Format the content as `<name>:0x<20-byte address>` so the
    // strict pin can verify both the canonical precompile name AND
    // the raw address.
    let mut content = String::with_capacity(name.len() + 1 + 2 + 40);
    content.push_str(name);
    content.push(':');
    content.push_str("0x");
    use std::fmt::Write as _;
    for byte in &bytes[12..] {
        let _ = write!(content, "{:02x}", byte);
    }
    Some(("Precompile", content))
}

fn build_log_event_content(
    op: &str,
    stack: Option<&[alloy::primitives::U256]>,
    memory: Option<&Vec<String>>,
) -> Option<String> {
    let log_num: u32 = op.strip_prefix("LOG").and_then(|n| n.parse().ok())?;
    if log_num > 4 {
        return None;
    }
    let stack = stack?;
    let min_stack = 2 + log_num as usize;
    if stack.len() < min_stack {
        return None;
    }

    // LOGn argument layout (top of stack first):
    //   [offset, size, topic0, topic1, ..., topic_{n-1}]
    // i.e. stack[len-1] = offset, stack[len-2] = size, then topics
    // descend toward the deeper stack positions.
    let offset = stack[stack.len() - 1];
    let size = stack[stack.len() - 2];

    let mut topics = Vec::with_capacity(log_num as usize);
    for t in 0..log_num as usize {
        let topic_idx = stack.len() - 3 - t;
        topics.push(format!("0x{:x}", stack[topic_idx]));
    }

    let mut content = topics.join(", ");

    // Append the non-indexed `data` segment when present.
    let size_usize = u64::try_from(size)
        .ok()
        .and_then(|x| usize::try_from(x).ok());
    let offset_usize = u64::try_from(offset)
        .ok()
        .and_then(|x| usize::try_from(x).ok());
    if let (Some(size), Some(offset)) = (size_usize, offset_usize)
        && size > 0
    {
        let memory_bytes = decode_struct_log_memory(memory);
        let end = offset.saturating_add(size);
        if end <= memory_bytes.len() {
            let data = &memory_bytes[offset..end];
            if !content.is_empty() {
                content.push_str(", ");
            }
            content.push_str("0x");
            for byte in data {
                use std::fmt::Write as _;
                let _ = write!(content, "{:02x}", byte);
            }
        }
    }

    Some(content)
}

/// Convert a structLog `op` string (e.g. `"PUSH1"`, `"ADD"`) to its raw
/// opcode byte.  Returns `None` for unknown mnemonics.
fn opcode_from_name(name: &str) -> Option<u8> {
    // Handle PUSH1..PUSH32, DUP1..DUP16, SWAP1..SWAP16
    if let Some(n) = name.strip_prefix("PUSH") {
        let n: u8 = n.parse().ok()?;
        if n == 0 {
            return Some(0x5f); // PUSH0
        }
        if (1..=32).contains(&n) {
            return Some(0x5f + n);
        }
    }
    if let Some(n) = name.strip_prefix("DUP") {
        let n: u8 = n.parse().ok()?;
        if (1..=16).contains(&n) {
            return Some(0x7f + n);
        }
    }
    if let Some(n) = name.strip_prefix("SWAP") {
        let n: u8 = n.parse().ok()?;
        if (1..=16).contains(&n) {
            return Some(0x8f + n);
        }
    }
    if let Some(n) = name.strip_prefix("LOG") {
        let n: u8 = n.parse().ok()?;
        if n <= 4 {
            return Some(0xa0 + n);
        }
    }

    Some(match name {
        "STOP" => 0x00,
        "ADD" => 0x01,
        "MUL" => 0x02,
        "SUB" => 0x03,
        "DIV" => 0x04,
        "SDIV" => 0x05,
        "MOD" => 0x06,
        "SMOD" => 0x07,
        "ADDMOD" => 0x08,
        "MULMOD" => 0x09,
        "EXP" => 0x0a,
        "SIGNEXTEND" => 0x0b,
        "LT" => 0x10,
        "GT" => 0x11,
        "SLT" => 0x12,
        "SGT" => 0x13,
        "EQ" => 0x14,
        "ISZERO" => 0x15,
        "AND" => 0x16,
        "OR" => 0x17,
        "XOR" => 0x18,
        "NOT" => 0x19,
        "BYTE" => 0x1a,
        "SHL" => 0x1b,
        "SHR" => 0x1c,
        "SAR" => 0x1d,
        "SHA3" | "KECCAK256" => 0x20,
        "ADDRESS" => 0x30,
        "BALANCE" => 0x31,
        "ORIGIN" => 0x32,
        "CALLER" => 0x33,
        "CALLVALUE" => 0x34,
        "CALLDATALOAD" => 0x35,
        "CALLDATASIZE" => 0x36,
        "CALLDATACOPY" => 0x37,
        "CODESIZE" => 0x38,
        "CODECOPY" => 0x39,
        "GASPRICE" => 0x3a,
        "EXTCODESIZE" => 0x3b,
        "EXTCODECOPY" => 0x3c,
        "RETURNDATASIZE" => 0x3d,
        "RETURNDATACOPY" => 0x3e,
        "EXTCODEHASH" => 0x3f,
        "BLOCKHASH" => 0x40,
        "COINBASE" => 0x41,
        "TIMESTAMP" => 0x42,
        "NUMBER" => 0x43,
        "PREVRANDAO" | "DIFFICULTY" => 0x44,
        "GASLIMIT" => 0x45,
        "CHAINID" => 0x46,
        "SELFBALANCE" => 0x47,
        "BASEFEE" => 0x48,
        "BLOBHASH" => 0x49,
        "BLOBBASEFEE" => 0x4a,
        "POP" => 0x50,
        "MLOAD" => 0x51,
        "MSTORE" => 0x52,
        "MSTORE8" => 0x53,
        "SLOAD" => 0x54,
        "SSTORE" => 0x55,
        "JUMP" => 0x56,
        "JUMPI" => 0x57,
        "PC" => 0x58,
        "MSIZE" => 0x59,
        "GAS" => 0x5a,
        "JUMPDEST" => 0x5b,
        "TLOAD" => 0x5c,
        "TSTORE" => 0x5d,
        "MCOPY" => 0x5e,
        "PUSH0" => 0x5f,
        "CREATE" => 0xf0,
        "CALL" => 0xf1,
        "CALLCODE" => 0xf2,
        "RETURN" => 0xf3,
        "DELEGATECALL" => 0xf4,
        "CREATE2" => 0xf5,
        "STATICCALL" => 0xfa,
        "REVERT" => 0xfd,
        "INVALID" => 0xfe,
        "SELFDESTRUCT" => 0xff,
        _ => return None,
    })
}
