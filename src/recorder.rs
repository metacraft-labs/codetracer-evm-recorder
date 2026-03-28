use codetracer_trace_types::{EventLogKind, Line, TypeKind, ValueRecord, TypeId};
use codetracer_trace_writer::trace_writer::TraceWriter;
use codetracer_trace_writer::{TraceEventsFileFormat, create_trace_writer};
use std::path::{Path, PathBuf};

use crate::solidity_ast::SolidityAst;
use crate::source_map::{self, JumpType, SourceMap};
use crate::stack_tracker::StackTracker;
use crate::storage_layout::StorageLayout;
use crate::structlog::StructLog;

/// Main EVM trace recorder. Processes EVM execution traces (structLog or
/// inspector-based) and writes them in CodeTracer's trace format.
pub struct EvmRecorder {
    writer: Box<dyn TraceWriter + Send>,
    type_names: Vec<String>,
    output_dir: PathBuf,
}

impl EvmRecorder {
    /// Create a new recorder targeting `output_dir`.
    pub fn new(program: &str, output_dir: &Path) -> eyre::Result<Self> {
        let writer = create_trace_writer(program, &[], TraceEventsFileFormat::Binary);
        Ok(Self {
            writer,
            type_names: Vec::new(),
            output_dir: output_dir.to_path_buf(),
        })
    }

    /// Initialize trace output files (trace.bin, trace_metadata.json, trace_paths.json).
    pub fn initialize(&mut self) -> eyre::Result<()> {
        let events_path = self.output_dir.join("trace.bin");
        let metadata_path = self.output_dir.join("trace_metadata.json");
        let paths_path = self.output_dir.join("trace_paths.json");

        TraceWriter::begin_writing_trace_events(&mut *self.writer, &events_path)
            .map_err(|e| eyre::eyre!("{}", e))?;
        TraceWriter::begin_writing_trace_metadata(&mut *self.writer, &metadata_path)
            .map_err(|e| eyre::eyre!("{}", e))?;
        TraceWriter::begin_writing_trace_paths(&mut *self.writer, &paths_path)
            .map_err(|e| eyre::eyre!("{}", e))?;

        self.register_evm_types();
        Ok(())
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
        TraceWriter::start(&mut *self.writer, main_path, Line(1));

        // The uint256 type id (first registered type) for storage values
        let uint256_type_id = TypeId(
            TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Int, "uint256").0,
        );

        let mut prev_line: Option<(i32, u32)> = None; // (file_index, line)
        let mut prev_depth: u64 = 1;

        // --- Local variable tracking (M5) ---
        // One StackTracker per call-stack depth.  We keep a small Vec indexed
        // by depth (depth 1 = index 0).  Resetting on depth changes keeps the
        // symbolic stack consistent with the real EVM stack.
        let mut stack_trackers: Vec<StackTracker> = Vec::new();

        for (i, log) in struct_logs.iter().enumerate() {
            let pc = log.pc as usize;

            // --- Depth changes: external calls / returns ---
            if log.depth > prev_depth {
                // Entering an external call (CALL/DELEGATECALL/STATICCALL)
                let fn_name = format!("external_call_depth_{}", log.depth);
                let call_path = prev_line
                    .and_then(|(fi, _)| source_paths.get(fi as usize))
                    .copied()
                    .unwrap_or(main_path);
                let call_line = prev_line.map(|(_, l)| Line(l as i64)).unwrap_or(Line(0));
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
            } else if log.depth < prev_depth {
                // Returning from an external call
                let depth_diff = prev_depth - log.depth;
                for _ in 0..depth_diff {
                    let ret_val = ValueRecord::Raw {
                        r: "0x".to_string(),
                        type_id: uint256_type_id,
                    };
                    TraceWriter::register_return(&mut *self.writer, ret_val);
                    // Pop the tracker for the exited depth.
                    stack_trackers.pop();
                }
            }

            // Ensure we always have a tracker for the current depth.
            while stack_trackers.len() < log.depth as usize {
                stack_trackers.push(StackTracker::new());
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
            if let Some(location) = source_map.resolve_pc(pc, &pc_to_idx, source_contents)
            {
                let file_idx = location.file_index;
                let line = location.line;
                let current = (file_idx, line);

                // Emit a step if the source line changed
                if prev_line != Some(current) {
                    let step_path = source_paths
                        .get(file_idx as usize)
                        .copied()
                        .unwrap_or(main_path);
                    TraceWriter::register_step(
                        &mut *self.writer,
                        step_path,
                        Line(line as i64),
                    );
                    prev_line = Some(current);
                }

                // --- Local variable emission (M5) ---
                // After each step we check the in-scope variables and emit
                // current values from the concrete stack.
                if let Some(op) = opcode {
                    if let (Some(ast), Some(src_off)) = (solidity_ast, source_offset) {
                        if let Some(func) = ast.function_at(src_off, file_idx) {
                            let in_scope = func.vars_in_scope_at(src_off);
                            // Update the symbolic tracker with in-scope variable info.
                            let _ = tracker.process_step(op, pc, Some(src_off), &in_scope);

                            // Emit current values for all in-scope variables.
                            if let Some(ref concrete_stack) = log.stack {
                                for var in &in_scope {
                                    if let Some(val) =
                                        tracker.get_variable_value(&var.name, concrete_stack)
                                    {
                                        let type_id = TraceWriter::ensure_type_id(
                                            &mut *self.writer,
                                            TypeKind::Int,
                                            &var.type_name,
                                        );
                                        let value_hex = format!("0x{:x}", val);
                                        let val_record = ValueRecord::Raw {
                                            r: value_hex,
                                            type_id,
                                        };
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
                if let Some(entry) =
                    source_map.get_entry_for_pc(pc, &pc_to_idx)
                {
                    match entry.jump_type {
                        JumpType::Into => {
                            // Internal function call
                            // Try to determine the target function from the
                            // next structLog entry's source location.
                            let fn_name = if let Some(next_log) =
                                struct_logs.get(i + 1)
                            {
                                if let Some(next_loc) = source_map.resolve_pc(
                                    next_log.pc as usize,
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
                            TraceWriter::register_call(
                                &mut *self.writer,
                                fn_id,
                                vec![],
                            );
                            // Reset the tracker when entering an internal function
                            // so we start fresh for the callee's locals.
                            tracker.reset();
                        }
                        JumpType::OutOf => {
                            // Internal function return
                            let ret_val = ValueRecord::Raw {
                                r: "0x".to_string(),
                                type_id: uint256_type_id,
                            };
                            TraceWriter::register_return(
                                &mut *self.writer,
                                ret_val,
                            );
                            tracker.reset();
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

            // --- SSTORE: decode storage writes ---
            if log.op.as_ref() == "SSTORE" {
                if let Some(ref stack) = log.stack {
                    // SSTORE pops [slot, value] from the stack.
                    // The top of the stack (last element) is the slot.
                    if stack.len() >= 2 {
                        let slot = stack[stack.len() - 1];
                        let value = stack[stack.len() - 2];

                        let slot_decimal = slot.to_string();
                        let value_hex = format!("0x{:x}", value);

                        // Try to resolve the variable name via storage layout
                        let var_name = storage_layout
                            .and_then(|sl| sl.resolve_slot(&slot_decimal))
                            .map(|entry| entry.label.clone())
                            .unwrap_or_else(|| format!("storage[{}]", slot_decimal));

                        let type_name = storage_layout
                            .and_then(|sl| sl.resolve_slot(&slot_decimal))
                            .and_then(|entry| {
                                storage_layout
                                    .and_then(|sl| sl.type_info(entry))
                                    .map(|ti| ti.label.clone())
                            })
                            .unwrap_or_else(|| "uint256".to_string());

                        let type_id = TraceWriter::ensure_type_id(
                            &mut *self.writer,
                            TypeKind::Int,
                            &type_name,
                        );

                        let val = ValueRecord::Raw {
                            r: value_hex,
                            type_id,
                        };
                        TraceWriter::register_variable_with_full_value(
                            &mut *self.writer,
                            &var_name,
                            val,
                        );
                    }
                }
            }

            // --- LOG0..LOG4: emit Solidity events ---
            if log.op.as_ref().starts_with("LOG") {
                let log_num = log.op.as_ref().strip_prefix("LOG")
                    .and_then(|n| n.parse::<u32>().ok())
                    .unwrap_or(0);

                if let Some(ref stack) = log.stack {
                    // LOGn pops: offset, size, topic0..topicN
                    // We capture the topics as event content.
                    let min_stack = 2 + log_num as usize;
                    if stack.len() >= min_stack {
                        let mut topics = Vec::new();
                        for t in 0..log_num as usize {
                            let topic_idx = stack.len() - 3 - t;
                            if topic_idx < stack.len() {
                                topics.push(format!("0x{:x}", stack[topic_idx]));
                            }
                        }
                        let content =
                            format!("LOG{}: {}", log_num, topics.join(", "));
                        TraceWriter::register_special_event(
                            &mut *self.writer,
                            EventLogKind::Write,
                            &content,
                        );
                    }
                }
            }

            prev_depth = log.depth;
        }

        Ok(())
    }

    /// Finalize the trace output, flushing all buffered data.
    pub fn finalize(&mut self) -> eyre::Result<()> {
        TraceWriter::finish_writing_trace_events(&mut *self.writer)
            .map_err(|e| eyre::eyre!("{}", e))?;
        TraceWriter::finish_writing_trace_metadata(&mut *self.writer)
            .map_err(|e| eyre::eyre!("{}", e))?;
        TraceWriter::finish_writing_trace_paths(&mut *self.writer)
            .map_err(|e| eyre::eyre!("{}", e))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper: map opcode mnemonic string to opcode byte
// ---------------------------------------------------------------------------

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
