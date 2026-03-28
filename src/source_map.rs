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
                        Some(Self::parse_jump(*s))
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
