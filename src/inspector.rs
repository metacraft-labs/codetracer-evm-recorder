//! CodeTracerInspector: a revm Inspector that captures EVM execution data
//! for later processing by the CodeTracer source mapping / recording layer.

use revm::interpreter::{
    CallInputs, CallOutcome, CallScheme, CreateInputs, CreateOutcome, Interpreter,
    InterpreterTypes,
    interpreter_types::{Jumps, MemoryTr, StackTr},
};
use revm::primitives::{Address, Log, U256};

/// A single captured EVM execution step.
#[derive(Debug, Clone)]
pub struct StepData {
    /// Program counter at the start of the step.
    pub pc: usize,
    /// Opcode byte.
    pub opcode: u8,
    /// Opcode name (e.g. "PUSH1", "CALL").
    pub opcode_name: String,
    /// Call depth at this step.
    /// Depth 1 = the outermost call or create frame (incremented by the `call`/`create`
    /// inspector hook before the first step of that frame executes).
    /// Depth 2 = a direct sub-call from the outermost frame, and so on.
    pub depth: u64,
    /// Stack contents (bottom to top) before the instruction executes.
    pub stack: Vec<U256>,
    /// Memory size before the instruction executes.
    pub memory_size: usize,
}

/// A call / create event captured by the inspector.
#[derive(Debug, Clone)]
pub struct CallData {
    /// Type of call: CALL, DELEGATECALL, STATICCALL, or CREATE/CREATE2.
    pub kind: CallKind,
    /// Caller address.
    pub caller: Address,
    /// Target (callee) address, or Address::ZERO for CREATEs (address unknown before execution).
    pub target: Address,
    /// Value transferred (zero for static / delegate calls).
    pub value: U256,
    /// Gas limit passed to the callee.
    pub gas_limit: u64,
}

/// Distinguishes between CALL variants and CREATE variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallKind {
    Call,
    /// Legacy CALLCODE opcode (deprecated since Homestead; behaves like CALL
    /// but executes in the caller's storage context with the caller's balance).
    CallCode,
    DelegateCall,
    StaticCall,
    Create,
    Create2,
}

/// A LOG event captured by the inspector.
#[derive(Debug, Clone)]
pub struct LogData {
    /// The emitting contract address.
    pub address: Address,
    /// Log topics (topic0 is the event signature for LOG1+).
    pub topics: Vec<revm::primitives::B256>,
    /// Log data bytes.
    pub data: revm::primitives::Bytes,
}

/// Collected execution data from a single transaction.
#[derive(Debug, Default)]
pub struct ExecutionData {
    /// All opcode steps in execution order.
    pub steps: Vec<StepData>,
    /// All call / create events in execution order.
    pub calls: Vec<CallData>,
    /// All LOG events in execution order.
    pub logs: Vec<LogData>,
    /// All selfdestruct events: (contract, beneficiary, value).
    pub selfdestructs: Vec<(Address, Address, U256)>,
}

impl ExecutionData {
    /// Returns true if no steps were recorded.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Returns the number of steps recorded.
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }
}

/// Inspector that records every EVM execution event into an `ExecutionData`
/// buffer for post-execution processing.
///
/// Usage:
/// ```ignore
/// let mut inspector = CodeTracerInspector::new();
/// // ... run EVM with this inspector ...
/// let data = inspector.into_execution_data();
/// // process data.steps, data.calls, data.logs etc.
/// ```
#[derive(Debug, Default)]
pub struct CodeTracerInspector {
    data: ExecutionData,
    /// Current call depth (incremented on call/create, decremented on call_end/create_end).
    current_depth: u64,
}

impl CodeTracerInspector {
    /// Create a new, empty inspector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume the inspector and return the collected execution data.
    pub fn into_execution_data(self) -> ExecutionData {
        self.data
    }

    /// Borrow the collected execution data.
    pub fn execution_data(&self) -> &ExecutionData {
        &self.data
    }
}

/// Convert an opcode byte to its human-readable name using revm's built-in table.
fn opcode_name(opcode: u8) -> String {
    use revm::state::bytecode::opcode::OpCode;
    if let Some(op) = OpCode::new(opcode) {
        format!("{op}")
    } else {
        format!("UNKNOWN_0x{opcode:02X}")
    }
}

impl<CTX, INTR> revm::Inspector<CTX, INTR> for CodeTracerInspector
where
    INTR: InterpreterTypes,
    INTR::Bytecode: Jumps,
    INTR::Stack: StackTr,
    INTR::Memory: MemoryTr,
{
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        let pc = interp.bytecode.pc();
        let opcode = interp.bytecode.opcode();
        let name = opcode_name(opcode);

        // Clone the full stack (bottom-to-top ordering).
        let stack: Vec<U256> = interp.stack.data().to_vec();
        let memory_size = interp.memory.size();

        self.data.steps.push(StepData {
            pc,
            opcode,
            opcode_name: name,
            depth: self.current_depth,
            stack,
            memory_size,
        });
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.current_depth += 1;

        let kind = match inputs.scheme {
            CallScheme::Call => CallKind::Call,
            CallScheme::DelegateCall => CallKind::DelegateCall,
            CallScheme::StaticCall => CallKind::StaticCall,
            // CALLCODE is a deprecated opcode (deprecated since Homestead).
            // It executes the callee's code in the caller's storage context,
            // similar to DELEGATECALL, but uses the caller's own balance for
            // value checks.  Map to the dedicated CallCode variant rather than
            // silently merging it with Call or DelegateCall.
            CallScheme::CallCode => CallKind::CallCode,
        };

        let value = inputs.transfer_value().unwrap_or(U256::ZERO);

        self.data.calls.push(CallData {
            kind,
            caller: inputs.caller,
            target: inputs.target_address,
            value,
            gas_limit: inputs.gas_limit,
        });

        None // don't override the call result
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        if self.current_depth > 0 {
            self.current_depth -= 1;
        }
    }

    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.current_depth += 1;

        let kind = match inputs.scheme() {
            revm::interpreter::CreateScheme::Create => CallKind::Create,
            revm::interpreter::CreateScheme::Create2 { .. } => CallKind::Create2,
            _ => CallKind::Create, // custom or future schemes
        };

        // The created contract address is not known until after execution;
        // record Address::ZERO as a placeholder.
        self.data.calls.push(CallData {
            kind,
            caller: inputs.caller(),
            target: Address::ZERO,
            value: inputs.value(),
            gas_limit: inputs.gas_limit(),
        });

        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        if self.current_depth > 0 {
            self.current_depth -= 1;
        }
    }

    fn log(&mut self, _context: &mut CTX, log: Log) {
        self.data.logs.push(LogData {
            address: log.address,
            topics: log.topics().to_vec(),
            data: log.data.data.clone(),
        });
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.data.selfdestructs.push((contract, target, value));
    }
}
