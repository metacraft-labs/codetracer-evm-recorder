/// Jump type encoded in a Solidity source map entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpType {
    /// 'i' -- entering a function call
    Into,
    /// 'o' -- returning from a function call
    OutOf,
    /// '-' -- regular instruction
    Regular,
}

/// A single entry in a Solidity/Vyper source map.
///
/// Format: `s:l:f:j:m` where values are incremental (an empty field means
/// "same as the previous entry").
#[derive(Debug, Clone)]
pub struct SourceMapEntry {
    /// Byte offset in the source file.
    pub offset: i32,
    /// Length (bytes) of the source range.
    pub length: i32,
    /// Source file index (-1 for compiler-generated code).
    pub file_index: i32,
    /// Jump type.
    pub jump_type: JumpType,
    /// Modifier depth.
    pub modifier_depth: i32,
}

impl Default for SourceMapEntry {
    fn default() -> Self {
        Self {
            offset: 0,
            length: 0,
            file_index: 0,
            jump_type: JumpType::Regular,
            modifier_depth: 0,
        }
    }
}

/// A resolved source location: file index plus line/column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    /// Source file index (from the source map entry).
    pub file_index: i32,
    /// 1-based line number within the source file.
    pub line: u32,
    /// 0-based column (byte offset within the line).
    pub column: u32,
}

/// Parsed Solidity/Vyper source map.
pub struct SourceMap {
    entries: Vec<SourceMapEntry>,
}

impl SourceMap {
    /// Parse a raw solc/vyper source map string.
    ///
    /// The format is semicolon-separated entries, each with colon-separated
    /// fields `s:l:f:j:m`. Empty fields inherit from the previous entry.
    pub fn parse(raw: &str) -> Self {
        let mut entries = Vec::new();
        let mut prev = SourceMapEntry::default();

        for part in raw.split(';') {
            let fields: Vec<&str> = part.split(':').collect();

            let offset = Self::parse_field(fields.first(), prev.offset);
            let length = Self::parse_field(fields.get(1), prev.length);
            let file_index = Self::parse_field(fields.get(2), prev.file_index);
            let jump_type = fields
                .get(3)
                .and_then(|s| {
                    if s.is_empty() {
                        None
                    } else {
                        Some(Self::parse_jump(s))
                    }
                })
                .unwrap_or(prev.jump_type);
            let modifier_depth = Self::parse_field(fields.get(4), prev.modifier_depth);

            let entry = SourceMapEntry {
                offset,
                length,
                file_index,
                jump_type,
                modifier_depth,
            };
            prev = entry.clone();
            entries.push(entry);
        }

        Self { entries }
    }

    /// Number of entries in the source map.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the source map is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get the source map entry for a given program-counter index.
    pub fn get(&self, pc: usize) -> Option<&SourceMapEntry> {
        self.entries.get(pc)
    }

    /// Resolve a bytecode PC (byte offset) to a source location.
    ///
    /// `pc_to_idx` is the mapping from PC offset to instruction index,
    /// built with [`build_pc_to_instruction_index`].
    /// `source_contents` is the array of source file contents indexed by
    /// `file_index`.
    ///
    /// Returns `None` if the PC or instruction index is out of range, or if
    /// the entry refers to compiler-generated code (file_index == -1).
    pub fn resolve_pc(
        &self,
        pc: usize,
        pc_to_idx: &[usize],
        source_contents: &[&str],
    ) -> Option<SourceLocation> {
        let &instr_idx = pc_to_idx.get(pc)?;
        let entry = self.entries.get(instr_idx)?;
        if entry.file_index < 0 {
            return None;
        }
        let file_idx = entry.file_index as usize;
        let source = source_contents.get(file_idx)?;
        let byte_offset = entry.offset as usize;
        if byte_offset > source.len() {
            return None;
        }
        let (line, column) = offset_to_line_col(source, byte_offset);
        Some(SourceLocation {
            file_index: entry.file_index,
            line,
            column,
        })
    }

    /// Get the source map entry for a bytecode PC, using a `pc_to_idx` mapping.
    pub fn get_entry_for_pc<'a>(
        &'a self,
        pc: usize,
        pc_to_idx: &[usize],
    ) -> Option<&'a SourceMapEntry> {
        let &instr_idx = pc_to_idx.get(pc)?;
        self.entries.get(instr_idx)
    }

    // -- private helpers --

    fn parse_field(field: Option<&&str>, prev: i32) -> i32 {
        match field {
            Some(s) if !s.is_empty() => s.parse::<i32>().unwrap_or(prev),
            _ => prev,
        }
    }

    fn parse_jump(s: &str) -> JumpType {
        match s {
            "i" => JumpType::Into,
            "o" => JumpType::OutOf,
            _ => JumpType::Regular,
        }
    }
}

/// Build a mapping from bytecode PC (byte offset) to instruction index.
///
/// EVM opcodes are variable-length: PUSH1..PUSH32 consume 1..32 extra bytes
/// after the opcode byte; all other opcodes are exactly 1 byte.
/// The source map is indexed by instruction position, so we need this mapping
/// to convert a PC from structLog into a source map index.
pub fn build_pc_to_instruction_index(bytecode: &[u8]) -> Vec<usize> {
    let mut pc_to_idx = vec![0usize; bytecode.len()];
    let mut pc = 0usize;
    let mut idx = 0usize;
    while pc < bytecode.len() {
        pc_to_idx[pc] = idx;
        let opcode = bytecode[pc];
        // PUSH1 = 0x60 .. PUSH32 = 0x7f
        if (0x60..=0x7f).contains(&opcode) {
            let push_bytes = (opcode - 0x60 + 1) as usize;
            // Fill the data bytes with the same instruction index
            // so that if a PC somehow points into push data we still
            // have a reasonable mapping.
            for offset in 1..=push_bytes {
                if pc + offset < bytecode.len() {
                    pc_to_idx[pc + offset] = idx;
                }
            }
            pc += 1 + push_bytes;
        } else {
            pc += 1;
        }
        idx += 1;
    }
    pc_to_idx
}

/// Convert a byte offset in source text to a (1-based line, 0-based column) pair.
fn offset_to_line_col(source: &str, byte_offset: usize) -> (u32, u32) {
    let mut line: u32 = 1;
    let mut col: u32 = 0;
    for (i, ch) in source.char_indices() {
        if i >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf8() as u32;
        }
    }
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_parse() {
        let map = SourceMap::parse("0:100:0:-:0;26:74:0;39:5:0;:11;44:1;:5:0:i;102:3:0:o");
        assert_eq!(map.len(), 7);

        let e0 = map.get(0).unwrap();
        assert_eq!(e0.offset, 0);
        assert_eq!(e0.length, 100);
        assert_eq!(e0.file_index, 0);
        assert_eq!(e0.jump_type, JumpType::Regular);
        assert_eq!(e0.modifier_depth, 0);

        // Entry 1 inherits modifier_depth=0 from previous
        let e1 = map.get(1).unwrap();
        assert_eq!(e1.offset, 26);
        assert_eq!(e1.length, 74);
        assert_eq!(e1.file_index, 0);
        assert_eq!(e1.modifier_depth, 0);

        // Entry 3: ":11" means offset inherited, length=11, rest inherited
        let e3 = map.get(3).unwrap();
        assert_eq!(e3.offset, 39); // inherited from entry 2
        assert_eq!(e3.length, 11);
        assert_eq!(e3.file_index, 0);

        // Entry 5 has jump_type Into
        let e5 = map.get(5).unwrap();
        assert_eq!(e5.jump_type, JumpType::Into);

        // Entry 6 has jump_type OutOf
        let e6 = map.get(6).unwrap();
        assert_eq!(e6.jump_type, JumpType::OutOf);
    }

    #[test]
    fn test_empty_input() {
        let map = SourceMap::parse("");
        // An empty string produces one entry (the empty split)
        assert_eq!(map.len(), 1);
    }
}
