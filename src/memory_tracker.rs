//! Memory tracker for *memory-escalated* local variables.
//!
//! While [`crate::stack_tracker::StackTracker`] handles Solidity locals that
//! fit in a 32-byte EVM stack slot (`uint*`, `bool`, `address`, ...), this
//! module tracks the locals that Solidity allocates in EVM memory:
//! `struct`s, `string`, `bytes`, dynamic arrays, and fixed-size arrays.
//!
//! # How Solidity allocates memory locals
//!
//! The Solidity calling convention reserves `memory[0x00..0x40]` as scratch
//! space and `memory[0x40..0x60]` for the **free memory pointer (FMP)**,
//! initialized to `0x80`.  When a function needs to allocate a memory local
//! of `size` bytes it emits roughly:
//!
//! ```text
//!   PUSH1 0x40        ; ... 0x40
//!   MLOAD             ; ... fmp                         <- base of new local
//!   ...               ; compute new fmp = fmp + size
//!   PUSH1 0x40        ; ... new_fmp 0x40
//!   MSTORE            ; bumps memory[0x40] to fmp + size
//! ```
//!
//! After this sequence the *old* FMP value (the base pointer of the freshly
//! allocated region) is left on the stack and Solidity then writes the
//! local's fields with `MSTORE`s relative to that base.
//!
//! # What this tracker does
//!
//! 1. Watches `MSTORE 0x40, X` instructions and uses the previous value of
//!    the free memory pointer as the base address of a newly allocated
//!    memory local.  The current source offset is matched against the AST's
//!    in-scope memory-resident `VarDecl`s to associate that base address
//!    with a variable name.
//! 2. Watches subsequent `MSTORE offset, value` instructions whose
//!    destination falls inside `[base, base + size)` of any tracked local
//!    and records the (offset, value) pair as a field assignment of that
//!    local.
//! 3. Given a structLog's memory snapshot, [`MemoryTracker::get_variable_value`]
//!    can surface the 32-byte word at a tracked variable's base, or
//!    [`MemoryTracker::get_field_value`] can read any 32-byte word inside
//!    the tracked region by relative offset.
//!
//! # Limitations
//!
//! - Targeted at *unoptimized* Solidity code.
//! - The size of each memory local is currently derived from the AST type
//!   string (`classify_type_size`).  Variable-length types (`string`,
//!   `bytes`, dynamic arrays) reserve their length-prefix slot but the
//!   payload size depends on runtime data and is left as `size_in_bytes`
//!   for the header only.
//! - The tracker is intentionally permissive: a memory write outside any
//!   known region is silently ignored.

use crate::solidity_ast::VarDecl;
use alloy::primitives::U256;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A detected MSTORE write into a memory-resident variable's region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemAssignment {
    /// Name of the source-level variable whose memory region was written.
    pub name: String,
    /// Absolute byte offset in EVM memory of the write.
    pub memory_offset: u64,
    /// Number of bytes written (32 for `MSTORE`, 1 for `MSTORE8`).
    pub size: usize,
}

/// One tracked memory-resident local: a name, its allocation base address
/// and how many bytes Solidity reserved for it at allocation time.
#[derive(Debug, Clone)]
pub struct MemRegion {
    /// Source-level name.
    pub name: String,
    /// Source-level Solidity type (e.g. `"struct S memory"`).
    pub type_name: String,
    /// Base offset of the region in EVM memory.
    pub base_offset: u64,
    /// Number of bytes reserved at allocation (the FMP bump delta).
    pub size_in_bytes: usize,
}

/// Offset of the **free memory pointer** in EVM memory (Solidity convention).
pub const FREE_MEMORY_POINTER_OFFSET: u64 = 0x40;

/// Default initial value of the free memory pointer (Solidity reserves the
/// first four words: `0x00..0x40` scratch + `0x40` FMP word + `0x60`
/// zero-slot, so user allocations start at `0x80`).
pub const INITIAL_FREE_MEMORY_POINTER: u64 = 0x80;

/// EVM opcodes we care about (subset, to avoid dragging in a full opcode
/// table).
const OP_MSTORE: u8 = 0x52;
const OP_MSTORE8: u8 = 0x53;
const OP_MLOAD: u8 = 0x51;

/// Conservative byte size for a single 32-byte memory word.
const WORD: usize = 32;

/// Tracks all live memory-resident locals for a given execution frame.
#[derive(Debug, Default)]
pub struct MemoryTracker {
    /// Active regions, in declaration order.
    regions: Vec<MemRegion>,
    /// Last seen value of the free memory pointer (read from the structLog's
    /// memory snapshot via [`MemoryTracker::observe_memory`]).  Cached so we
    /// can detect FMP-bump allocations.
    last_fmp: u64,
}

impl MemoryTracker {
    /// Create an empty tracker.  The first call to [`Self::observe_memory`]
    /// will initialise the cached FMP.
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
            last_fmp: INITIAL_FREE_MEMORY_POINTER,
        }
    }

    /// Reset to an empty state (call on function entry / return).
    pub fn reset(&mut self) {
        self.regions.clear();
        self.last_fmp = INITIAL_FREE_MEMORY_POINTER;
    }

    /// Current number of tracked regions.  Mostly useful for assertions.
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Iterate over the currently tracked regions.
    pub fn regions(&self) -> &[MemRegion] {
        &self.regions
    }

    /// Look up a tracked region by name (most-recently-registered wins).
    pub fn region(&self, name: &str) -> Option<&MemRegion> {
        self.regions.iter().rev().find(|r| r.name == name)
    }

    /// Read the latest free-memory-pointer value from a memory snapshot.
    ///
    /// `memory` is a flat byte slice — i.e. the concatenation of the
    /// 32-byte words in structLog's `memory` field.  When the snapshot is
    /// shorter than `0x60` bytes (e.g. before any allocation), the tracker
    /// keeps its previous cached value.
    pub fn observe_memory(&mut self, memory: &[u8]) {
        if memory.len() < (FREE_MEMORY_POINTER_OFFSET + WORD as u64) as usize {
            return;
        }
        let start = FREE_MEMORY_POINTER_OFFSET as usize;
        let word = &memory[start..start + WORD];
        // The FMP fits comfortably in u64 in practice; truncate the high
        // bytes (they must be zero for a legitimate pointer).
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&word[WORD - 8..WORD]);
        self.last_fmp = u64::from_be_bytes(buf);
    }

    /// Process one EVM opcode step and return any memory-variable
    /// assignments newly observed at this step.
    ///
    /// # Arguments
    ///
    /// * `opcode` — raw opcode byte at this step.
    /// * `_pc` — program counter (currently unused).
    /// * `source_offset` — Solidity source-map offset for the instruction.
    /// * `vars_in_scope` — variables visible at `source_offset` (passed
    ///   through to allocation detection).
    /// * `pre_stack` — the EVM stack *before* the opcode executes (matches
    ///   structLog's `stack` field on the same step).
    /// * `pre_memory` — the EVM memory *before* the opcode executes.
    pub fn process_step(
        &mut self,
        opcode: u8,
        _pc: u64,
        source_offset: Option<i32>,
        vars_in_scope: &[&VarDecl],
        pre_stack: &[U256],
        pre_memory: &[u8],
    ) -> Vec<MemAssignment> {
        // Always refresh the FMP cache from the latest pre-step snapshot
        // so we can recognise FMP-bump allocations even when the bump is
        // performed via copy-paste assembly (Solidity occasionally uses
        // CALLDATACOPY etc.).
        let prev_fmp = self.last_fmp;
        self.observe_memory(pre_memory);

        match opcode {
            OP_MSTORE => self.handle_mstore(source_offset, vars_in_scope, pre_stack, prev_fmp),
            OP_MSTORE8 => self.handle_mstore8(pre_stack),
            // MLOAD does not produce a new assignment, but we still want
            // to refresh the FMP if it happens to touch 0x40 — already
            // handled above.
            OP_MLOAD => Vec::new(),
            _ => Vec::new(),
        }
    }

    /// Read a tracked variable's *header word* (first 32 bytes of its
    /// region) from a memory snapshot.
    pub fn get_variable_value(&self, name: &str, memory: &[u8]) -> Option<U256> {
        let region = self.region(name)?;
        read_word(memory, region.base_offset as usize)
    }

    /// Read a 32-byte word at `field_offset` *within* the named variable's
    /// region.  Returns `None` if the variable is not tracked, the offset
    /// is past the region's allocated size, or the memory snapshot is
    /// too short.
    pub fn get_field_value(
        &self,
        name: &str,
        field_offset: u64,
        memory: &[u8],
    ) -> Option<U256> {
        let region = self.region(name)?;
        if (field_offset as usize + WORD) > region.size_in_bytes {
            return None;
        }
        read_word(memory, (region.base_offset + field_offset) as usize)
    }

    /// Return `(name, header_value)` for every tracked region, dropping
    /// regions whose memory snapshot is too short.
    pub fn all_variable_values(&self, memory: &[u8]) -> Vec<(String, U256)> {
        let mut out = Vec::new();
        for r in &self.regions {
            if let Some(v) = read_word(memory, r.base_offset as usize) {
                out.push((r.name.clone(), v));
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    /// Handle `MSTORE offset, value`.
    ///
    /// Two cases:
    ///
    /// * `offset == 0x40`: this is a free-memory-pointer bump.  The *new*
    ///   FMP is on top of the stack and the *old* FMP is `self.last_fmp`
    ///   (from the pre-step snapshot).  Register a region for any
    ///   memory-resident variable whose source range covers the current
    ///   `source_offset`.
    /// * any other offset: if it falls inside an existing region, emit a
    ///   `MemAssignment` describing that field write.
    fn handle_mstore(
        &mut self,
        source_offset: Option<i32>,
        vars_in_scope: &[&VarDecl],
        pre_stack: &[U256],
        prev_fmp: u64,
    ) -> Vec<MemAssignment> {
        // MSTORE pops (offset, value).  In structLog convention the stack
        // is bottom-to-top, so the top-of-stack is `len - 1` (offset) and
        // `len - 2` is the value.
        if pre_stack.len() < 2 {
            return Vec::new();
        }
        let dest_u = pre_stack[pre_stack.len() - 1];
        let dest = u256_to_u64_saturating(dest_u);

        if dest == FREE_MEMORY_POINTER_OFFSET {
            let new_fmp = u256_to_u64_saturating(pre_stack[pre_stack.len() - 2]);
            self.handle_fmp_bump(prev_fmp, new_fmp, source_offset, vars_in_scope);
            return Vec::new();
        }

        // Otherwise: is this write inside any tracked region?
        let mut assigns = Vec::new();
        for r in &self.regions {
            if dest >= r.base_offset && dest + WORD as u64 <= r.base_offset + r.size_in_bytes as u64
            {
                assigns.push(MemAssignment {
                    name: r.name.clone(),
                    memory_offset: dest,
                    size: WORD,
                });
                break;
            }
        }
        assigns
    }

    fn handle_mstore8(&mut self, pre_stack: &[U256]) -> Vec<MemAssignment> {
        // MSTORE8 writes a single byte; we still want to surface
        // assignments to `bytes`/`string` tails.
        if pre_stack.len() < 2 {
            return Vec::new();
        }
        let dest = u256_to_u64_saturating(pre_stack[pre_stack.len() - 1]);
        let mut assigns = Vec::new();
        for r in &self.regions {
            if dest >= r.base_offset && dest < r.base_offset + r.size_in_bytes as u64 {
                assigns.push(MemAssignment {
                    name: r.name.clone(),
                    memory_offset: dest,
                    size: 1,
                });
                break;
            }
        }
        assigns
    }

    /// React to a free-memory-pointer bump: the difference between the old
    /// and new FMP is the allocation size, and any memory-resident
    /// variable visible at the current source offset gets a region rooted
    /// at the old FMP.
    fn handle_fmp_bump(
        &mut self,
        prev_fmp: u64,
        new_fmp: u64,
        source_offset: Option<i32>,
        vars_in_scope: &[&VarDecl],
    ) {
        if new_fmp <= prev_fmp {
            return; // not an allocation
        }
        let size = (new_fmp - prev_fmp) as usize;

        // Pick a candidate variable: prefer the memory-resident var whose
        // declaration / statement range contains `source_offset`.  Fall
        // back to the most-recently-declared memory-resident var in scope
        // (this matches the unoptimized solc emission pattern where the
        // allocation immediately follows the declaration).
        let candidate = source_offset
            .and_then(|off| pick_var_for_offset(off, vars_in_scope))
            .or_else(|| {
                vars_in_scope
                    .iter()
                    .rev()
                    .find(|v| v.is_memory_resident() && !self.is_already_registered(&v.name))
                    .copied()
            });

        if let Some(v) = candidate {
            if self.is_already_registered(&v.name) {
                return;
            }
            self.regions.push(MemRegion {
                name: v.name.clone(),
                type_name: v.type_name.clone(),
                base_offset: prev_fmp,
                size_in_bytes: size,
            });
        }
    }

    fn is_already_registered(&self, name: &str) -> bool {
        self.regions.iter().any(|r| r.name == name)
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Read a big-endian 32-byte word at `offset` of `memory`, padding with
/// zeros if the snapshot is shorter than `offset + 32` (mirrors the EVM's
/// implicit zero-extension on MLOAD).
fn read_word(memory: &[u8], offset: usize) -> Option<U256> {
    if memory.is_empty() && offset != 0 {
        return None;
    }
    let mut buf = [0u8; WORD];
    let end = offset.saturating_add(WORD);
    let avail_end = end.min(memory.len());
    if offset >= memory.len() {
        // Entirely zero — but only meaningful if the caller knew this
        // region was allocated; return Some(0) since the EVM would return
        // 0 for an MLOAD here.
        return Some(U256::ZERO);
    }
    let avail = &memory[offset..avail_end];
    buf[..avail.len()].copy_from_slice(avail);
    Some(U256::from_be_bytes(buf))
}

fn u256_to_u64_saturating(v: U256) -> u64 {
    // We saturate at u64::MAX to avoid panicking on absurd values that
    // would never legitimately appear as a memory offset.
    let limbs = v.as_limbs();
    if limbs[1] != 0 || limbs[2] != 0 || limbs[3] != 0 {
        return u64::MAX;
    }
    limbs[0]
}

/// Pick the memory-resident variable whose declaration/statement range
/// contains the given source offset.  Mirrors the prioritisation logic
/// used by [`crate::stack_tracker::StackTracker::label_for_push`].
fn pick_var_for_offset<'a>(
    offset: i32,
    vars_in_scope: &'a [&'a VarDecl],
) -> Option<&'a VarDecl> {
    // 1) exact declaration offset
    for v in vars_in_scope {
        if v.is_memory_resident() && v.declaration_offset == offset {
            return Some(*v);
        }
    }
    // 2) inside the VariableDeclaration src range
    for v in vars_in_scope {
        if v.is_memory_resident() && v.src.contains_offset(offset) {
            return Some(*v);
        }
    }
    // 3) inside the statement range (covers initializer / first MSTOREs)
    for v in vars_in_scope {
        if v.is_memory_resident()
            && let Some(stmt) = &v.statement_range
            && stmt.contains_offset(offset)
        {
            return Some(*v);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solidity_ast::{SourceRange, VarDecl};

    fn make_var(name: &str, type_name: &str, offset: i32, length: i32) -> VarDecl {
        VarDecl {
            name: name.to_string(),
            type_name: type_name.to_string(),
            src: SourceRange {
                offset,
                length,
                file_index: 0,
            },
            declaration_offset: offset,
            statement_range: Some(SourceRange {
                offset,
                length: length + 30,
                file_index: 0,
            }),
        }
    }

    /// Build a memory snapshot with the FMP word at 0x40 set to `fmp`.
    fn mem_with_fmp(size_bytes: usize, fmp: u64) -> Vec<u8> {
        let mut m = vec![0u8; size_bytes.max(0x60)];
        let off = FREE_MEMORY_POINTER_OFFSET as usize;
        m[off..off + WORD].copy_from_slice(&U256::from(fmp).to_be_bytes::<32>());
        m
    }

    fn write_word_at(mem: &mut Vec<u8>, at: u64, value: U256) {
        let needed = at as usize + WORD;
        if mem.len() < needed {
            mem.resize(needed, 0);
        }
        mem[at as usize..at as usize + WORD].copy_from_slice(&value.to_be_bytes::<32>());
    }

    #[test]
    fn fmp_bump_registers_region() {
        let mut t = MemoryTracker::new();
        let var = make_var("s", "struct S memory", 100, 20);

        // Pre-step memory: FMP = 0x80
        let mem = mem_with_fmp(0x60, 0x80);
        // Stack for MSTORE: bottom -> [..., new_fmp=0xC0, dst=0x40]
        let stack = vec![U256::from(0xC0u64), U256::from(0x40u64)];

        let asgn = t.process_step(OP_MSTORE, 0, Some(100), &[&var], &stack, &mem);
        assert!(asgn.is_empty(), "FMP bump itself doesn't emit assignments");

        let r = t.region("s").expect("region registered");
        assert_eq!(r.base_offset, 0x80);
        assert_eq!(r.size_in_bytes, 0x40);
    }

    #[test]
    fn field_mstore_inside_region_is_reported() {
        let mut t = MemoryTracker::new();
        let var = make_var("s", "struct S memory", 100, 20);

        // 1) Allocation: FMP 0x80 -> 0xC0
        let mut mem = mem_with_fmp(0x60, 0x80);
        let alloc_stack = vec![U256::from(0xC0u64), U256::from(0x40u64)];
        let _ = t.process_step(OP_MSTORE, 0, Some(100), &[&var], &alloc_stack, &mem);

        // 2) After the bump, memory[0x40] becomes 0xC0.
        write_word_at(&mut mem, FREE_MEMORY_POINTER_OFFSET, U256::from(0xC0u64));

        // 3) Write `value=42` at offset 0x80 (the `a` field of the struct).
        let store_stack = vec![U256::from(42u64), U256::from(0x80u64)];
        let asgns = t.process_step(OP_MSTORE, 4, Some(110), &[&var], &store_stack, &mem);
        assert_eq!(asgns.len(), 1);
        assert_eq!(asgns[0].name, "s");
        assert_eq!(asgns[0].memory_offset, 0x80);
        assert_eq!(asgns[0].size, WORD);
    }

    #[test]
    fn classify_skips_value_types() {
        let v = make_var("x", "uint256", 0, 9);
        assert!(v.is_stack_resident());
        assert!(!v.is_memory_resident());
    }

    #[test]
    fn classify_recognises_struct_memory() {
        let v = make_var("s", "struct Pair memory", 0, 30);
        assert!(v.is_memory_resident());
    }

    #[test]
    fn read_word_zero_pads() {
        let mem = vec![0u8; 0x40];
        assert_eq!(read_word(&mem, 0x100), Some(U256::ZERO));
    }
}
