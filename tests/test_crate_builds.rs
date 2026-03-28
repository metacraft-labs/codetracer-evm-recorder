use codetracer_evm_recorder::recorder::EvmRecorder;
use tempfile::TempDir;

#[test]
fn test_crate_builds() {
    // Verify the crate compiles and key types are accessible
    let tmp = TempDir::new().unwrap();
    let recorder = EvmRecorder::new("test", tmp.path()).unwrap();
    // Basic smoke test that all dependencies are wired up correctly
    drop(recorder);
}

#[test]
fn test_source_map_parsing() {
    use codetracer_evm_recorder::source_map::{JumpType, SourceMap};

    // Real solc source map fragment
    let raw = "0:100:0:-:0;26:74:0;39:5:0;:11;44:1;:5:0:i;102:3:0:o";
    let map = SourceMap::parse(raw);

    assert!(map.len() > 0);
    let first = map.get(0).unwrap();
    assert_eq!(first.offset, 0);
    assert_eq!(first.length, 100);
    assert_eq!(first.file_index, 0);
    assert_eq!(first.jump_type, JumpType::Regular);
}

#[test]
fn test_storage_layout_parsing() {
    use codetracer_evm_recorder::storage_layout::StorageLayout;

    let json = r#"{
        "storage": [
            {"astId": 1, "contract": "FlowTest", "label": "storedA", "offset": 0, "slot": "0", "type": "t_uint256"},
            {"astId": 2, "contract": "FlowTest", "label": "storedResult", "offset": 0, "slot": "1", "type": "t_uint256"}
        ],
        "types": {
            "t_uint256": {"encoding": "inplace", "label": "uint256", "numberOfBytes": "32"}
        }
    }"#;

    let layout: StorageLayout = serde_json::from_str(json).unwrap();
    assert_eq!(layout.storage.len(), 2);
    assert_eq!(layout.storage[0].label, "storedA");
    assert_eq!(layout.storage[0].slot, "0");
    assert_eq!(layout.storage[1].label, "storedResult");
    assert_eq!(layout.storage[1].slot, "1");
}
