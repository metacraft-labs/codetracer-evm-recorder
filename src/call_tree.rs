//! Call tree construction for multi-contract trace analysis.
//!
//! As execution moves across contract boundaries (CALL, DELEGATECALL,
//! STATICCALL, CREATE, CREATE2) we build a tree that mirrors the call stack.
//! Each [`CallNode`] records the target address, call type, source-step range,
//! and any nested calls made from within that frame.

use alloy::primitives::Address;

// ---------------------------------------------------------------------------
// CallType
// ---------------------------------------------------------------------------

/// The kind of external call or contract creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallType {
    /// Regular ETH transfer / contract call (CALL opcode).
    Call,
    /// Delegated call — runs implementation code in caller's storage context.
    DelegateCall,
    /// Read-only call (STATICCALL opcode).
    StaticCall,
    /// Contract deployment (CREATE opcode).
    Create,
    /// Deterministic contract deployment (CREATE2 opcode).
    Create2,
    /// Top-level transaction entry point.
    Transaction,
}

// ---------------------------------------------------------------------------
// CallNode
// ---------------------------------------------------------------------------

/// A node in the call trace tree representing one call frame.
#[derive(Debug)]
pub struct CallNode {
    /// Contract address being executed in this frame.
    pub address: Address,
    /// Human-readable function name, if known from source map / AST.
    pub function_name: String,
    /// How this frame was entered.
    pub call_type: CallType,
    /// Calls made from within this frame (in order of occurrence).
    pub children: Vec<CallNode>,
    /// `(start_step, end_step)` — indices into the original `struct_logs`
    /// slice.  `end_step` is set when the frame exits (initially 0).
    pub step_range: (usize, usize),
}

impl CallNode {
    fn new(address: Address, call_type: CallType, start_step: usize) -> Self {
        Self {
            address,
            function_name: String::new(),
            call_type,
            children: Vec::new(),
            step_range: (start_step, 0),
        }
    }
}

// ---------------------------------------------------------------------------
// CallTree
// ---------------------------------------------------------------------------

/// A tree of call frames built incrementally as depth changes are observed.
///
/// The tree is constructed by calling [`enter_call`] when the EVM depth
/// increases and [`exit_call`] when it decreases.  The root is the
/// top-level transaction frame.
pub struct CallTree {
    /// The root node (top-level transaction frame).
    root: CallNode,
    /// Stack of indices that tracks the path from root to the current node.
    /// Each element is an index into the `children` array of its parent.
    /// When empty we are at the root.
    path: Vec<usize>,
}

impl CallTree {
    /// Create a new call tree rooted at `root_address` (the transaction target).
    pub fn new(root_address: Address) -> Self {
        Self {
            root: CallNode::new(root_address, CallType::Transaction, 0),
            path: Vec::new(),
        }
    }

    /// Push a new call frame (called when structLog `depth` increases).
    pub fn enter_call(&mut self, address: Address, call_type: CallType, step: usize) {
        let child = CallNode::new(address, call_type, step);
        let parent = self.current_mut();
        let child_idx = parent.children.len();
        parent.children.push(child);
        self.path.push(child_idx);
    }

    /// Close the current call frame (called when structLog `depth` decreases).
    ///
    /// If the path is already empty (we are at the root), this is a no-op:
    /// an unbalanced exit (e.g. caused by SELFDESTRUCT or malformed trace
    /// data) must not corrupt the tree.
    pub fn exit_call(&mut self, step: usize) {
        if self.path.is_empty() {
            // Already at root — nothing to pop.  The root's end step is set
            // at the final step by the caller, not here.
            return;
        }
        // Mark the end step on the node that is about to be popped.
        self.current_mut().step_range.1 = step;
        self.path.pop();
    }

    /// Returns `true` if the current frame is the root (no pending child frames).
    pub fn is_at_root(&self) -> bool {
        self.path.is_empty()
    }

    /// Immutable reference to the currently-executing call node.
    pub fn current(&self) -> &CallNode {
        let mut node = &self.root;
        for &idx in &self.path {
            node = &node.children[idx];
        }
        node
    }

    /// Immutable reference to the root node.
    pub fn root(&self) -> &CallNode {
        &self.root
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    fn current_mut(&mut self) -> &mut CallNode {
        let mut node = &mut self.root;
        for &idx in &self.path {
            node = &mut node.children[idx];
        }
        node
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(hex: &str) -> Address {
        hex.parse().unwrap()
    }

    #[test]
    fn test_call_tree_construction() {
        let root_addr = addr("0x1111111111111111111111111111111111111111");
        let child_addr = addr("0x2222222222222222222222222222222222222222");
        let grandchild_addr = addr("0x3333333333333333333333333333333333333333");

        let mut tree = CallTree::new(root_addr);

        // Verify initial state
        assert_eq!(tree.current().address, root_addr);
        assert_eq!(tree.current().call_type, CallType::Transaction);
        assert!(tree.current().children.is_empty());

        // Enter a CALL at step 5
        tree.enter_call(child_addr, CallType::Call, 5);
        assert_eq!(tree.current().address, child_addr);
        assert_eq!(tree.current().step_range.0, 5);

        // Enter a DELEGATECALL at step 10 from within child
        tree.enter_call(grandchild_addr, CallType::DelegateCall, 10);
        assert_eq!(tree.current().address, grandchild_addr);
        assert_eq!(tree.current().call_type, CallType::DelegateCall);

        // Exit the grandchild at step 15
        tree.exit_call(15);
        assert_eq!(tree.current().address, child_addr);

        // Exit the child at step 20
        tree.exit_call(20);
        assert_eq!(tree.current().address, root_addr);

        // Verify the tree structure
        let root = tree.root();
        assert_eq!(root.address, root_addr);
        assert_eq!(root.children.len(), 1);

        let child = &root.children[0];
        assert_eq!(child.address, child_addr);
        assert_eq!(child.step_range, (5, 20));
        assert_eq!(child.children.len(), 1);

        let grandchild = &child.children[0];
        assert_eq!(grandchild.address, grandchild_addr);
        assert_eq!(grandchild.step_range, (10, 15));
        assert_eq!(grandchild.call_type, CallType::DelegateCall);
        assert!(grandchild.children.is_empty());
    }

    #[test]
    fn test_multiple_sibling_calls() {
        let root_addr = addr("0x1111111111111111111111111111111111111111");
        let child_a = addr("0x2222222222222222222222222222222222222222");
        let child_b = addr("0x3333333333333333333333333333333333333333");

        let mut tree = CallTree::new(root_addr);

        // First child call
        tree.enter_call(child_a, CallType::Call, 1);
        tree.exit_call(5);

        // Second child call
        tree.enter_call(child_b, CallType::StaticCall, 10);
        tree.exit_call(15);

        let root = tree.root();
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].address, child_a);
        assert_eq!(root.children[0].call_type, CallType::Call);
        assert_eq!(root.children[0].step_range, (1, 5));

        assert_eq!(root.children[1].address, child_b);
        assert_eq!(root.children[1].call_type, CallType::StaticCall);
        assert_eq!(root.children[1].step_range, (10, 15));
    }

    #[test]
    fn test_create_call_type() {
        let root_addr = addr("0x1111111111111111111111111111111111111111");
        let created_addr = addr("0x4444444444444444444444444444444444444444");

        let mut tree = CallTree::new(root_addr);
        tree.enter_call(created_addr, CallType::Create, 3);
        assert_eq!(tree.current().call_type, CallType::Create);
        tree.exit_call(8);

        assert_eq!(tree.root().children[0].call_type, CallType::Create);
    }

    #[test]
    fn test_exit_call_at_root_is_noop() {
        // Calling exit_call when already at the root (path empty) must not
        // panic or corrupt the tree.  This models SELFDESTRUCT or a malformed
        // trace that emits more exits than enters.
        let root_addr = addr("0x1111111111111111111111111111111111111111");
        let child_addr = addr("0x2222222222222222222222222222222222222222");

        let mut tree = CallTree::new(root_addr);

        // Single enter/exit — now at root.
        tree.enter_call(child_addr, CallType::Call, 1);
        tree.exit_call(5);
        assert!(tree.is_at_root());
        assert_eq!(tree.current().address, root_addr);

        // Extra exit — must be a no-op.
        tree.exit_call(10);
        assert!(tree.is_at_root());
        assert_eq!(tree.current().address, root_addr);
        // Root's step_range should be unchanged (not overwritten by the extra exit).
        assert_eq!(tree.root().step_range.0, 0);
        // The child's step_range must still be correct.
        assert_eq!(tree.root().children[0].step_range, (1, 5));
    }

    #[test]
    fn test_is_at_root() {
        let root_addr = addr("0x1111111111111111111111111111111111111111");
        let child_addr = addr("0x2222222222222222222222222222222222222222");

        let mut tree = CallTree::new(root_addr);
        assert!(tree.is_at_root());

        tree.enter_call(child_addr, CallType::Call, 1);
        assert!(!tree.is_at_root());

        tree.exit_call(5);
        assert!(tree.is_at_root());
    }

    #[test]
    fn test_selfdestruct_like_multi_exit() {
        // Simulate a depth drop of 2 (two frames exit simultaneously), which
        // can happen with SELFDESTRUCT inside a called contract.
        let root_addr = addr("0x1111111111111111111111111111111111111111");
        let child_addr = addr("0x2222222222222222222222222222222222222222");
        let grandchild_addr = addr("0x3333333333333333333333333333333333333333");

        let mut tree = CallTree::new(root_addr);
        tree.enter_call(child_addr, CallType::Call, 1);
        tree.enter_call(grandchild_addr, CallType::DelegateCall, 3);

        // Both frames exit at once (depth drops by 2).
        tree.exit_call(10);
        tree.exit_call(10);

        // Now at root.
        assert!(tree.is_at_root());
        assert_eq!(tree.current().address, root_addr);

        // Verify both child nodes have their end steps set.
        assert_eq!(tree.root().children[0].step_range.1, 10);
        assert_eq!(tree.root().children[0].children[0].step_range.1, 10);
    }
}
