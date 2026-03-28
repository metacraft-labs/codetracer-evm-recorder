use codetracer_trace_types::TypeKind;
use codetracer_trace_writer::trace_writer::TraceWriter;
use codetracer_trace_writer::{TraceEventsFileFormat, create_trace_writer};
use std::path::{Path, PathBuf};

use crate::source_map::SourceMap;
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

    /// Process a sequence of structLog entries with the given source map and
    /// emit corresponding trace events.
    pub fn record_from_structlog(
        &mut self,
        _struct_logs: &[StructLog],
        _source_map: &SourceMap,
    ) -> eyre::Result<()> {
        // TODO (M1): implement structLog -> trace event mapping
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
