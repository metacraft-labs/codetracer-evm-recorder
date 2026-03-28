//! Registry mapping contract addresses to their compilation artifacts.
//!
//! When tracing multi-contract transactions, the recorder must switch source
//! maps, AST and storage layouts as execution crosses contract boundaries.
//! This module provides the central lookup table for those artifacts.

use std::collections::HashMap;
use std::path::PathBuf;

use alloy::primitives::Address;

use crate::solidity_ast::SolidityAst;
use crate::source_map::SourceMap;
use crate::storage_layout::StorageLayout;

// ---------------------------------------------------------------------------
// ContractArtifacts
// ---------------------------------------------------------------------------

/// Compilation artifacts for a single deployed contract.
pub struct ContractArtifacts {
    /// Human-readable contract name.
    pub name: String,
    /// Parsed source map for the deployed (runtime) bytecode.
    pub source_map: SourceMap,
    /// Deployed bytecode bytes (used to build PC-to-instruction mapping).
    pub runtime_bytecode: Vec<u8>,
    /// PC-to-instruction-index mapping derived from `runtime_bytecode`.
    pub pc_to_idx: Vec<usize>,
    /// Source file paths (indexed by the source map's file_index).
    pub source_paths: Vec<PathBuf>,
    /// Source file contents (indexed by the source map's file_index).
    pub source_contents: Vec<String>,
    /// Optional Solidity storage layout (for decoding SSTORE operations).
    pub storage_layout: Option<StorageLayout>,
    /// Optional Solidity AST (for local variable reconstruction).
    pub solidity_ast: Option<SolidityAst>,
}

// ---------------------------------------------------------------------------
// DelegateCallView
// ---------------------------------------------------------------------------

/// A combined view used when processing a DELEGATECALL:
///
/// - Source map / AST / bytecode come from the *implementation* contract
///   (the code being executed).
/// - Storage layout comes from the *proxy* contract (the storage context).
pub struct DelegateCallView<'a> {
    pub source_map: &'a SourceMap,
    pub pc_to_idx: &'a [usize],
    pub source_paths: &'a [PathBuf],
    pub source_contents: &'a [String],
    /// Storage layout from the proxy contract.
    pub storage_layout: Option<&'a StorageLayout>,
    /// AST from the implementation contract.
    pub solidity_ast: Option<&'a SolidityAst>,
}

// ---------------------------------------------------------------------------
// ContractRegistry
// ---------------------------------------------------------------------------

/// Maps contract addresses to their compilation artifacts.
///
/// Also supports a *default* address used as a fallback when an exact address
/// match is not found (typically the main/entry-point contract).
pub struct ContractRegistry {
    contracts: HashMap<Address, ContractArtifacts>,
    /// Address used as fallback when the exact address is unknown.
    default_address: Option<Address>,
}

impl ContractRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            contracts: HashMap::new(),
            default_address: None,
        }
    }

    /// Register artifacts for a contract at a known address.
    pub fn register(&mut self, address: Address, artifacts: ContractArtifacts) {
        self.contracts.insert(address, artifacts);
    }

    /// Set the default contract address.
    ///
    /// The default is used by [`get`] when no entry exists for the requested
    /// address.  This is typically set to the main contract being traced.
    pub fn set_default(&mut self, address: Address) {
        self.default_address = Some(address);
    }

    /// Look up artifacts for `address`, falling back to the default.
    ///
    /// Returns `None` only if neither `address` nor the default are registered.
    pub fn get(&self, address: &Address) -> Option<&ContractArtifacts> {
        if let Some(a) = self.contracts.get(address) {
            return Some(a);
        }
        if let Some(ref default_addr) = self.default_address {
            return self.contracts.get(default_addr);
        }
        None
    }

    /// Look up artifacts for a DELEGATECALL.
    ///
    /// A DELEGATECALL runs the *implementation* contract's bytecode but
    /// operates on the *proxy* contract's storage.  The returned view
    /// combines:
    ///
    /// - Source map, bytecode, source files, AST — from `implementation`.
    /// - Storage layout — from `proxy`.
    ///
    /// Returns `None` if the implementation contract is not in the registry.
    pub fn get_delegatecall<'a>(
        &'a self,
        implementation: &Address,
        proxy: &Address,
    ) -> Option<DelegateCallView<'a>> {
        let impl_artifacts = self.get(implementation)?;
        let proxy_storage = self
            .contracts
            .get(proxy)
            .and_then(|a| a.storage_layout.as_ref());

        Some(DelegateCallView {
            source_map: &impl_artifacts.source_map,
            pc_to_idx: &impl_artifacts.pc_to_idx,
            source_paths: &impl_artifacts.source_paths,
            source_contents: &impl_artifacts.source_contents,
            storage_layout: proxy_storage,
            solidity_ast: impl_artifacts.solidity_ast.as_ref(),
        })
    }
}

impl Default for ContractRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_map::SourceMap;

    fn make_artifacts(name: &str) -> ContractArtifacts {
        ContractArtifacts {
            name: name.to_string(),
            source_map: SourceMap::parse("0:10:0"),
            runtime_bytecode: vec![0x60, 0x00, 0x56], // PUSH1 0x00 JUMP
            pc_to_idx: vec![0, 0, 1, 2],
            source_paths: vec![PathBuf::from(format!("{}.sol", name))],
            source_contents: vec![format!("// {}", name)],
            storage_layout: None,
            solidity_ast: None,
        }
    }

    #[test]
    fn test_contract_registry() {
        let addr_a: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let addr_b: Address = "0x2222222222222222222222222222222222222222"
            .parse()
            .unwrap();
        let addr_unknown: Address = "0x9999999999999999999999999999999999999999"
            .parse()
            .unwrap();

        let mut registry = ContractRegistry::new();
        registry.register(addr_a, make_artifacts("ContractA"));
        registry.register(addr_b, make_artifacts("ContractB"));
        registry.set_default(addr_a);

        // Exact lookup
        assert_eq!(registry.get(&addr_a).map(|a| &a.name), Some(&"ContractA".to_string()));
        assert_eq!(registry.get(&addr_b).map(|a| &a.name), Some(&"ContractB".to_string()));

        // Unknown address falls back to default (ContractA)
        assert_eq!(
            registry.get(&addr_unknown).map(|a| &a.name),
            Some(&"ContractA".to_string())
        );
    }

    #[test]
    fn test_registry_no_default() {
        let addr_a: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let addr_unknown: Address = "0x9999999999999999999999999999999999999999"
            .parse()
            .unwrap();

        let mut registry = ContractRegistry::new();
        registry.register(addr_a, make_artifacts("ContractA"));

        // No default set — unknown address returns None
        assert!(registry.get(&addr_unknown).is_none());
    }

    #[test]
    fn test_delegatecall_view() {
        let proxy_addr: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let impl_addr: Address = "0x2222222222222222222222222222222222222222"
            .parse()
            .unwrap();

        let storage_json = r#"{
            "storage": [
                {"astId": 1, "contract": "Proxy", "label": "counter", "offset": 0, "slot": "0", "type": "t_uint256"}
            ],
            "types": {
                "t_uint256": {"encoding": "inplace", "label": "uint256", "numberOfBytes": "32"}
            }
        }"#;

        let mut proxy_artifacts = make_artifacts("Proxy");
        proxy_artifacts.storage_layout =
            Some(crate::storage_layout::StorageLayout::from_json(storage_json).unwrap());

        let impl_artifacts = make_artifacts("Implementation");

        let mut registry = ContractRegistry::new();
        registry.register(proxy_addr, proxy_artifacts);
        registry.register(impl_addr, impl_artifacts);

        // DelegateCall view: implementation source + proxy storage
        let view = registry.get_delegatecall(&impl_addr, &proxy_addr).unwrap();

        // Source paths come from the implementation
        assert_eq!(view.source_paths[0], PathBuf::from("Implementation.sol"));

        // Storage layout comes from the proxy
        let layout = view.storage_layout.unwrap();
        assert_eq!(layout.storage[0].label, "counter");

        // Unknown implementation returns None
        let unknown_addr: Address = "0x9999999999999999999999999999999999999999"
            .parse()
            .unwrap();
        assert!(registry.get_delegatecall(&unknown_addr, &proxy_addr).is_none());
    }
}
