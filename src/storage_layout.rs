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

    /// Resolve a storage slot number (decimal string) to the corresponding
    /// storage entry, if any.
    pub fn resolve_slot(&self, slot: &str) -> Option<&StorageEntry> {
        self.storage.iter().find(|e| e.slot == slot)
    }

    /// Get the type info for a storage entry.
    pub fn type_info(&self, entry: &StorageEntry) -> Option<&StorageTypeInfo> {
        self.types.get(&entry.type_name)
    }

    /// Look up the qualified `<struct>.<member>` name for a storage
    /// slot that lives inside an `inplace`-encoded struct.
    ///
    /// Returns `Some((parent_label, member_label))` when `slot` falls
    /// inside the contiguous slot range owned by a struct declared in
    /// the storage layout AND the slot offset relative to the struct
    /// base matches one of the struct's named members.
    ///
    /// Used by the recorder's SSTORE encoder to surface struct-field
    /// writes under their canonical `<struct>.<field>` names instead
    /// of synthetic `storage[<slot>]` placeholders (M11 category 2).
    pub fn struct_member_at(&self, slot: u64) -> Option<(String, String)> {
        for entry in &self.storage {
            let Some(base_slot) = entry.slot.parse::<u64>().ok() else {
                continue;
            };
            let Some(ti) = self.type_info(entry) else {
                continue;
            };
            if ti.encoding != "inplace" {
                continue;
            }
            let Some(members) = ti.members.as_ref() else {
                continue;
            };
            // Walk the members; pick the one whose absolute slot
            // matches `slot`.
            for member in members {
                let member_offset: u64 = match member.slot.parse() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if base_slot.saturating_add(member_offset) == slot {
                    return Some((entry.label.clone(), member.label.clone()));
                }
            }
        }
        None
    }

    /// Look up the `<array>[<index>]` qualified name for a storage
    /// slot that lives inside a fixed-size array.
    ///
    /// Returns `Some((parent_label, index))` when `slot` falls inside
    /// a fixed-size array's contiguous slot range.  The index is the
    /// 0-based element position computed from `slot - base_slot`.
    pub fn array_element_at(&self, slot: u64) -> Option<(String, u64)> {
        for entry in &self.storage {
            let Some(base_slot) = entry.slot.parse::<u64>().ok() else {
                continue;
            };
            let Some(ti) = self.type_info(entry) else {
                continue;
            };
            if ti.encoding != "inplace" {
                continue;
            }
            // Need a `base` (array element type) to be a fixed-size array.
            if ti.base.is_none() {
                continue;
            }
            let Some(total_bytes) = ti.number_of_bytes.parse::<u64>().ok() else {
                continue;
            };
            let slot_count = total_bytes.div_ceil(32);
            if slot >= base_slot && slot < base_slot + slot_count {
                return Some((entry.label.clone(), slot - base_slot));
            }
        }
        None
    }

    /// Find the mapping declared at a specific base slot (the slot
    /// number embedded in the `keccak256(key . slot)` hash that
    /// derives a mapping value's storage location).
    ///
    /// Returns `Some((label, key_type_label))` when the layout
    /// declares a mapping at `base_slot`; the `key_type_label` is
    /// looked up via the mapping's `key` type info (e.g. `"address"`,
    /// `"uint256"`).  Used to surface mapping writes under their
    /// canonical `<name>[<key>]` names (M11 category 2).
    pub fn mapping_at_base(&self, base_slot: u64) -> Option<(String, String)> {
        for entry in &self.storage {
            let Some(entry_slot) = entry.slot.parse::<u64>().ok() else {
                continue;
            };
            if entry_slot != base_slot {
                continue;
            }
            let Some(ti) = self.type_info(entry) else {
                continue;
            };
            if ti.encoding != "mapping" {
                continue;
            }
            let Some(key_type_id) = ti.key.as_deref() else {
                continue;
            };
            let key_label = self
                .types
                .get(key_type_id)
                .map(|kt| kt.label.clone())
                .unwrap_or_else(|| "uint256".to_string());
            return Some((entry.label.clone(), key_label));
        }
        None
    }

    /// Look up the *containing* compound storage entry (a fixed-size
    /// array or an `inplace`-encoded struct) whose slot range covers
    /// `slot`.
    ///
    /// Returns `Some((entry, type_info, slot_count))` when `slot` falls
    /// inside the contiguous slot range owned by an array or struct
    /// declared in the storage layout.  `slot_count` is the number of
    /// 32-byte slots the compound occupies (`length` for an array,
    /// `members.len()` for a struct).
    ///
    /// Mappings are NOT covered: their entries live at hashed slots
    /// derived from the key, not in a contiguous range, so we cannot
    /// reconstruct a `Sequence`/`Struct` value from the layout alone.
    pub fn containing_compound(&self, slot: u64) -> Option<(&StorageEntry, &StorageTypeInfo, u64)> {
        for entry in &self.storage {
            let Some(base_slot) = entry.slot.parse::<u64>().ok() else {
                continue;
            };
            let Some(ti) = self.type_info(entry) else {
                continue;
            };
            let Some(total_bytes) = ti.number_of_bytes.parse::<u64>().ok() else {
                continue;
            };
            // Solidity packs primitives smaller than 32 bytes into a
            // single slot, but for arrays of 32-byte elements and for
            // `inplace`-encoded structs each member starts at its own
            // slot.  We round up to whole slots.
            let slot_count = total_bytes.div_ceil(32);
            if slot_count <= 1 {
                continue;
            }
            if ti.encoding != "inplace" {
                continue;
            }
            // Only arrays or structs are interesting (must have members
            // or a base element type).
            if ti.members.is_none() && ti.base.is_none() {
                continue;
            }
            if slot >= base_slot && slot < base_slot + slot_count {
                return Some((entry, ti, slot_count));
            }
        }
        None
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
