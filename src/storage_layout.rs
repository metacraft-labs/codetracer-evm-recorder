use serde::Deserialize;
use std::collections::HashMap;

/// Parsed representation of solc's `storageLayout` JSON output.
#[derive(Debug, Deserialize)]
pub struct StorageLayout {
    /// Ordered list of storage variable entries.
    pub storage: Vec<StorageEntry>,
    /// Map from type identifier (e.g. `"t_uint256"`) to type metadata.
    pub types: HashMap<String, StorageTypeInfo>,
}

/// A single storage variable entry from the storageLayout.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageEntry {
    /// Variable name as written in the source.
    pub label: String,
    /// Storage slot number (decimal string).
    pub slot: String,
    /// Byte offset within the 32-byte slot (for packed variables).
    pub offset: u64,
    /// Type identifier referencing `StorageLayout::types`.
    #[serde(rename = "type")]
    pub type_name: String,
    /// AST node id (optional, present in solc output).
    #[serde(default)]
    #[serde(rename = "astId")]
    pub ast_id: Option<u64>,
    /// Contract name (optional).
    #[serde(default)]
    pub contract: Option<String>,
}

/// Type metadata from the storageLayout types map.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageTypeInfo {
    /// Encoding strategy: "inplace", "mapping", "dynamic_array", etc.
    pub encoding: String,
    /// Human-readable type name (e.g. "uint256").
    pub label: String,
    /// Size in bytes (as a decimal string).
    #[serde(rename = "numberOfBytes")]
    pub number_of_bytes: String,
    /// For mappings: key type identifier.
    #[serde(default)]
    pub key: Option<String>,
    /// For mappings: value type identifier.
    #[serde(default)]
    pub value: Option<String>,
    /// For arrays: base element type identifier.
    #[serde(default)]
    pub base: Option<String>,
    /// For structs: member entries.
    #[serde(default)]
    pub members: Option<Vec<StorageEntry>>,
}

impl StorageLayout {
    /// Parse a storageLayout from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Look up a storage entry by variable name.
    pub fn find_by_label(&self, label: &str) -> Option<&StorageEntry> {
        self.storage.iter().find(|e| e.label == label)
    }

    /// Get the type info for a storage entry.
    pub fn type_info(&self, entry: &StorageEntry) -> Option<&StorageTypeInfo> {
        self.types.get(&entry.type_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_layout() {
        let json = r#"{
            "storage": [
                {"astId": 1, "contract": "FlowTest", "label": "storedA", "offset": 0, "slot": "0", "type": "t_uint256"},
                {"astId": 2, "contract": "FlowTest", "label": "storedResult", "offset": 0, "slot": "1", "type": "t_uint256"}
            ],
            "types": {
                "t_uint256": {"encoding": "inplace", "label": "uint256", "numberOfBytes": "32"}
            }
        }"#;

        let layout = StorageLayout::from_json(json).unwrap();
        assert_eq!(layout.storage.len(), 2);
        assert_eq!(layout.storage[0].label, "storedA");
        assert_eq!(layout.storage[0].slot, "0");
        assert_eq!(layout.storage[1].label, "storedResult");
        assert_eq!(layout.storage[1].slot, "1");

        let entry = layout.find_by_label("storedA").unwrap();
        let ti = layout.type_info(entry).unwrap();
        assert_eq!(ti.label, "uint256");
        assert_eq!(ti.number_of_bytes, "32");
    }
}
