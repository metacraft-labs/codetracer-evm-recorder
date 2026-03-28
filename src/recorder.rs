use codetracer_trace_types::{EventLogKind, Line, TypeKind, ValueRecord, TypeId};
use codetracer_trace_writer::trace_writer::TraceWriter;
use codetracer_trace_writer::{TraceEventsFileFormat, create_trace_writer};
use std::path::{Path, PathBuf};

use crate::source_map::{self, JumpType, SourceMap};
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
    /// bytecode, source file contents, source file paths, and optional
    /// storage layout, emitting corresponding trace events.
    ///
    /// # Arguments
    ///
    /// * `struct_logs` - The structLog entries from `debug_traceTransaction`.
    /// * `source_map` - Parsed source map for the deployed bytecode.
    /// * `bytecode` - The deployed bytecode (used to build PC-to-instruction mapping).
    /// * `source_paths` - Paths of the source files (indexed by file_index).
    /// * `source_contents` - Contents of the source files (indexed by file_index).
    /// * `storage_layout` - Optional storage layout for decoding SSTORE operations.
    pub fn record_from_structlog(
        &mut self,
        struct_logs: &[StructLog],
        source_map: &SourceMap,
        bytecode: &[u8],
        source_paths: &[&Path],
        source_contents: &[&str],
        storage_layout: Option<&StorageLayout>,
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
            } else if log.depth < prev_depth {
                // Returning from an external call
                let depth_diff = prev_depth - log.depth;
                for _ in 0..depth_diff {
                    let ret_val = ValueRecord::Raw {
                        r: "0x".to_string(),
                        type_id: uint256_type_id,
                    };
                    TraceWriter::register_return(&mut *self.writer, ret_val);
                }
            }

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
                        }
                        JumpType::Regular => {}
                    }
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
