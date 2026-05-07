# EVM Recorder CTFS Audit — 2026-05-01

This audit checks the `codetracer-evm-recorder` against the canonical
CodeTracer multi-stream CTFS schema and the section 5.6 audit checklist
maintained in `/tmp/isonim-migration.txt` (see handoff entries 1.21 / 1.22 /
1.27 / 1.30 / 1.38).  The previously-audited Ruby (1.21, 1.22), Python (1.27)
and JavaScript (1.38) recorders set the canonical fix patterns.

## Summary

| # | Check | Status | Notes |
|---|---|---|---|
| a | `register_call` for each call | OK (post-fix) | Internal Solidity calls now use AST-resolved function names (`add` etc.) instead of `fn_at_pc_<N>` placeholders.  External call frames at depth changes already routed through `register_call`. |
| b | Call args via `register_call_arg` / `arg` | **PARTIAL** | Internal Solidity calls now seed callee parameter labels from the concrete EVM stack at `JumpType::Into` and stage each `(name, value)` through `TraceWriter::arg` before `register_call`.  CTFS readback still reports empty `Call.args`, pinning the remaining writer-side attachment gap. |
| c | Write / WriteOther for stdout/stderr | N/A — fixed semantically | EVM has no stdout/stderr.  LOG opcodes now route through `register_special_event(EvmEvent, "LOG{n}", topics)` instead of the `Write` mis-tag, matching the Stylus tracer convention and the frontend's `EventLogKind::EvmEvent` rendering path. |
| d | Thread events (ThreadStart / Exit / Switch) | OK | EVM is single-threaded; recorder correctly emits no thread events.  Smoke-test guard rails this against future regressions. |
| e | Step records for line navigation | OK | `register_step(path, line)` is called on every source-line change; e2e traces produce >5 steps for `FlowTest::compute()`. |
| f | Canonical CTFS schema match | **OK (post-fix)** | Recorder now uses `TraceEventsFileFormat::Ctfs` (multi-stream `steps.dat` + `calls.dat` + `events.dat` + `funcs.dat`) so the canonical `NimTraceReaderHandle` and db-backend `CTFSTraceReader` consume traces directly.  Previously used `Json` which produced an old-format container the structured readers cannot decode. |
| g | Obsolete `#[no_mangle]` stubs | OK | None present (this recorder pre-dates the JS-recorder pattern that introduced the FFI stubs that conflicted with upstream Nim exports). |

## Concrete fixes applied

### 1. Switched output container to canonical CTFS multi-stream

`EvmRecorder::new` now passes `TraceEventsFileFormat::Ctfs` to
`create_trace_writer`.  The previous comment claimed the binary path
"produced incorrect results when read by db-backend (empty locals
despite valid data)" — that referred to the legacy CBOR+Zstd format
("Binary"), not the modern CTFS multi-stream container.  The CTFS
variant is the format used by the Ruby and Python native recorders
post-2026-04 (handoff entries 1.21, 1.22, 1.27).

Result: the produced `.ct` now contains `steps.dat`, `calls.dat`,
`events.dat`, `funcs.dat`, `types.dat`, etc., so the `NimTraceReaderHandle`
can index events directly without a postprocess pass.

### 2. EVM `LOG{n}` opcodes routed through `EventLogKind::EvmEvent`

LOG opcodes were previously emitted as `EventLogKind::Write`
(stdout-style writes), which made them mix with terminal output from
recorders like Python and Ruby.  EVM has no stdout concept; LOG events
are structured contract events.

The fix uses `EventLogKind::EvmEvent` (numeric 13), matching the Stylus
tracer convention checked by
`codetracer/src/db-backend/tests/stylus_flow_integration.rs`.  The
metadata field carries the opcode mnemonic (`LOG0`..`LOG4`) and the
content carries the indexed topics.

Note: the Nim multi-stream IO event stream collapses the 13-variant
`EventLogKind` into 4 buckets (`stdout`, `stderr`, `fileOp`, `error`)
via `toIOEventKind` in `codetracer_trace_writer_ffi.nim`.  In that
collapsed view, `EvmEvent` lands in the `stderr` bucket, not the
`stdout` bucket where `Write` would have sent it.  This is enough to
keep EVM logs out of the terminal-output pane.  An infrastructure
follow-up to preserve `EvmEvent` end-to-end through the multi-stream
format is tracked in `/tmp/isonim-migration.txt` § 5.6.

### 3. Internal call function names resolved via Solidity AST

When a `JumpType::Into` source-map entry is encountered, the recorder
now scans the next 5 structlog entries' source offsets and matches them
against `SolidityAst::function_at`.  This replaces the legacy
placeholder `fn_at_<file>:<line>` (or `fn_at_pc_<N>`) with the actual
Solidity function name.  The lookahead handles solc-generated function
prologues (`JUMPDEST` + `PUSH`/`POP` opcodes) at the head of every
internal function that have no source mapping (`file_index == -1`).

Falls back gracefully to the previous placeholder when the AST is
unavailable or the offset doesn't fall inside any known function.

## Tests added

`tests/test_ctfs_audit.rs` (4 audit tests, all passing under
`cargo test --release`):

- `audit_ctfs_internal_call_emitted` — opens the recorded `.ct` via
  the `NimTraceReaderHandle` FFI and asserts that at least one
  `Call.function_id` resolves to `add` (verifying the AST-aware
  function-name fix).
- `audit_ctfs_log_event_kind_is_evmevent` — asserts every Event
  record from the recorder lands in the `stderr` IO bucket
  (`EvmEvent` collapse target), with no events leaking into the
  `stdout` bucket where `Write` would have sent them.
- `audit_ctfs_step_records_emitted` — asserts at least 5 Step
  records are produced for `FlowTest::compute()` so source-line
  navigation works end-to-end.
- `audit_ctfs_linked_writer_staged_args_roundtrip` — proves the
  exact `codetracer_trace_writer_nim` dependency linked into this
  recorder can attach `TraceWriter::arg("x"/"y", Raw(...))` to the next
  `register_call(add)` in a minimal CTFS trace.
- `audit_ctfs_call_args_writer_gap_known_empty` — pins the remaining
  EVM lifecycle gap: even though the linked writer attaches staged args
  in isolation, the internal `add(x, y)` call in the real EVM trace
  currently reads back with empty CTFS `Call.args`.

## Concrete partial fix applied in follow-up

### 4. Internal-call `Call.args` staged from the callee stack

The follow-up keeps the AST lookahead from the original audit, but
adds explicit stack-label seeding for already-live callee parameters.
At `JumpType::Into`, after resolving the target `FunctionDef`, the
recorder reads the current JUMP instruction's pre-execution concrete
stack.  Solidity internal-call parameters sit immediately below the
jump destination in source parameter order, so the recorder stages each
value with `TraceWriter::arg(param.name, ValueRecord::Raw{...})` before
emitting `register_call`.

`StackTracker::seed_top_labels(stack_depth, labels)` mirrors the same
mapping in the symbolic tracker.  That keeps parameter locals visible
after the function entry instead of resetting the tracker to an empty
stack.  If the AST or concrete stack is unavailable, the recorder still
emits the call and resets the tracker; it does not fabricate args.

The fix is applied to both `record_from_structlog` and
`record_from_structlog_multi_contract`.  A targeted diagnostic run
confirmed the recorder recovered and staged `x=0xa` and `y=0x14` for
`FlowTest.add(uint256,uint256)`, but `NimTraceReaderHandle::call_json`
still returned `"args":[]`.

An additional focused diagnostic now asserts that `x` and `y` are present
in the CTFS varname table while the `add` call record still has empty
`args`.  That proves the EVM source-level parameter recovery and the
variable-value side of `TraceWriter::arg` are live in the produced `.ct`;
the missing layer is specifically the pending-call-argument attachment
that `trace_writer_register_call` is supposed to consume.

The latest diagnostic run also proves the staged arguments are not simply
being consumed by an earlier call: the first completed call record is the
internal `add` call itself, and every call record in the trace still reports
zero args.  The loss is therefore between the EVM-side
`TraceWriter::arg("x"/"y", ...)` staging call and the next
`trace_writer_register_call(add)` writing a populated `CallRecord.args`
entry, not later call-key lookup or accidental attachment to a sibling call.

The newest linked-writer guard closes one suspected dependency branch:
the sibling `codetracer_trace_writer_nim` crate and linked Nim archive
do attach `TraceWriter::arg("x"/"y", Raw(...))` to the immediately
following `register_call(add)` in a synthetic CTFS trace.  The remaining
loss is therefore specific to the real EVM recording lifecycle/order
around the internal `JumpType::Into` call, not a stale static library or
generic Rust Nim-writer staged-arg failure.

Remaining next-layer fix shape: instrument the EVM `JumpType::Into`
path around `stage_internal_call_args` and the following return sequence,
then reduce the synthetic writer guard until it includes the missing
EVM ordering ingredient (surrounding steps/values, absorbed top-level
call, nested `fn_at_pc_*` calls, or return timing).  Once isolated,
replace
`audit_ctfs_call_args_writer_gap_known_empty` with a positive assertion
that `add(x, y)` carries two args named `x` and `y`.

## Open gaps / follow-ups

### Multi-stream `EvmEvent` semantics

The Nim multi-stream IO stream only preserves 4 IO event kinds
(`stdout` / `stderr` / `fileOp` / `error`).  The 13-variant
`EventLogKind` is collapsed; LOG metadata is also dropped.  For the
EVM frontend's `event_log.nim` and `flow.nim` to render LOG events
distinctly from terminal stderr writes, the multi-stream IO event
stream would need to either preserve the original kind byte or be
replaced by a more expressive stream type.

This is an infrastructure change in
`codetracer-trace-format-nim/src/codetracer_trace_writer_ffi.nim` and
`codetracer_trace_writer/io_event_stream.nim`; out of scope for this
recorder audit.

### Internal-call `register_return` value

`register_return` is currently called with a placeholder
`Raw{r:"0x"}` value.  Capturing the actual return value would require
reading the next post-OutOf step's stack — straightforward but not
implemented.  Nice-to-have, not blocking.

### Unaudited code path: `record_from_structlog_multi_contract`

The audit smoke tests exercise `record_from_structlog`
(single-contract) only.  The `record_from_structlog_multi_contract`
path received the same fixes (LOG → EvmEvent, AST function-name
resolution) but its behaviour is verified only through the
pre-existing e2e tests in `tests/test_e2e_vars.rs` (which all still
pass).  Adding a smoke test that exercises a contract-with-DELEGATECALL
fixture would close this gap.

## Convention compliance follow-up — 2026-05-08

Closes the `--out-dir` / env-var gap flagged in
`codetracer-specs/Recorder-CLI-Conventions.md` §3 / §5 (the
"Implementation Status" table previously listed the EVM recorder as
"⚠ Partial — uses `--trace-dir`"). Cairo precedent: commit `2710b5e`
in `codetracer-cairo-recorder` (2026-05-08).  This recorder was
already CTFS-only post-audit (see "1. Switched output container..."
above), so the fix here is narrower than the cairo / cardano /
circom rollups — no `--format` to drop.

### 1. `--trace-dir` → `--out-dir` (with deprecated alias)

`src/main.rs` now accepts the canonical `--out-dir <PATH>` /
`-o <PATH>` flag.  `--trace-dir <PATH>` is kept as a hidden
deprecated alias so existing scripts (and any in-tree wrappers that
still call `--trace-dir`) keep working — when used, the CLI emits a
one-line stderr note:

```
warning: --trace-dir is deprecated, use --out-dir
```

The `record_trace` helper internals (`trace_dir` Rust local) were
renamed to `out_dir` throughout for consistency with the flag.

### 2. Standard env vars added (§5)

Two recorder-scoped env vars now drive the CLI:

* `CODETRACER_EVM_RECORDER_OUT_DIR` — fallback for `--out-dir`.  CLI
  flag always wins; deprecated `--trace-dir` flag also wins if given.
* `CODETRACER_EVM_RECORDER_DISABLED` — set to `1`/`true` to skip
  recording entirely.  The EVM recorder doesn't run a separate
  target subprocess (it spins up Anvil and calls the contract
  itself), so "disabled" simply means "don't emit any trace
  artefacts and skip the Anvil round-trip".  The recorder still
  exits 0.

Resolution order matches the cairo/cardano/circom precedent:

1. `--out-dir` (canonical).
2. `--trace-dir` (deprecated alias; emits stderr note).
3. `CODETRACER_EVM_RECORDER_OUT_DIR` env var.
4. Error (no usable default — explicit output dir required).

`resolve_out_dir()` and `recording_disabled()` helpers mirror the
cairo precedent.

### 3. `--help` mentions `ct print`

The clap `long_about` now points users at `ct print` from
`codetracer-trace-format-nim` for human-readable conversion of the
recorded `.ct` bundle, and documents the two env vars.  This is the
contract that `Recorder-CLI-Conventions.md` §4 requires for
CTFS-only recorders.

### 4. Tests added (`tests/test_cli_convention.rs`, 6 tests)

* `test_no_format_flag_in_help` — guards against accidentally adding
  a `--format` flag (CTFS is the only on-disk shape; this is a
  positive forward-going regression rail).
* `test_help_mentions_ct_print` — guards the §4 contract.
* `test_env_out_dir_used_when_flag_omitted` — exercises the env-var
  fallback through a real recorder run against `FlowTest.sol`.
* `test_env_disabled_skips_recording` — exercises the disabled mode
  (no Anvil spin-up, no `.ct` written, exit 0).  Doesn't require
  solc/anvil because the disabled path short-circuits before any
  toolchain calls.
* `test_trace_dir_alias_still_works_with_deprecation_note` —
  exercises the legacy alias and asserts the deprecation note lands
  on stderr.
* `test_recorded_trace_via_ct_print_json` — records `FlowTest.sol`,
  pipes the produced `.ct` through `ct print --json`, and asserts on
  **structural anchors** (source filename, contract program label,
  the internal `add` Solidity helper in the function table, and the
  `storedA`/`storedResult` storage-variable names in the varname
  table).  Falls back to structural anchors instead of integer
  values because EVM recorder Variable record payloads don't
  round-trip through `ct print --json` today (same pre-existing
  limitation as Cardano 1.48 / Circom 1.49; tracked elsewhere as a
  recorder-internal follow-up).

### 5. No-silent-skip guard
(`tests/verify-cli-convention-no-silent-skip.sh`)

A bash-level guard asserts the CLI continues to comply with the
convention even if Rust-level tests are bypassed:

* `--format` and `CODETRACER_FORMAT` are absent from `--help`
  (top-level and `record`).
* `--out-dir`, `--version`, and `ct print` are present in the
  appropriate `--help` outputs.
* `CODETRACER_EVM_RECORDER_OUT_DIR` and
  `CODETRACER_EVM_RECORDER_DISABLED` are referenced in `src/`
  (i.e. the env-var fallbacks cannot be silently removed).

Wired into `Justfile`'s `lint` and `test` recipes (and the
standalone `verify-cli-convention` recipe).

### 6. Implementation Status table updated

`codetracer-specs/Recorder-CLI-Conventions.md` now lists the EVM
recorder row as `✓ Compliant (CTFS-only)` with the
`CODETRACER_EVM_RECORDER_OUT_DIR` /
`CODETRACER_EVM_RECORDER_DISABLED` env vars enumerated, matching
the cairo / cardano / circom rows.
