//! Per-program `ct-print --full` strict-assertion coverage tests.
//!
//! These tests follow the recorder-test-requirements policy
//! (`metacraft-specs/policies/recorder-test-requirements.md`):
//!
//! * Each test records one Solidity program through the recorder's
//!   normal entry point — `codetracer-evm-recorder record <file>` —
//!   on a transient anvil node.
//! * The produced `.ct` is piped through `ct-print --full --strip-paths`.
//! * Assertions are made on the **decoded JSON document** with EXACT
//!   counts (`assert_eq!(events.len(), N)` — never `>=`), EXACT
//!   ordering (the per-event `step_index` strictly increases; the
//!   step-line vector is asserted as a complete equality), and EXACT
//!   decoded values (`value["r"] == "0xa"`, `value["kind"] == "Raw"`).
//!
//! `ValueRecord` variants outside the expected set are rejected with
//! a hard error message asking the test author to extend the test
//! rather than weaken the assertion.
//!
//! Where the recorder's current behaviour deviates from what the
//! Solidity semantics dictate (e.g. internal-function names not being
//! resolved via the Solidity AST — the recorder falls back to
//! `fn_at_pc_<n>` placeholders — or `EvmEvent` records being routed
//! through the multi-stream `ioStderr` channel rather than a dedicated
//! EVM event channel), the deviation is documented inline as
//! `RECORDER BUG: ...` and a parallel `#[ignore]`d assertion captures
//! the spec-correct expectation so it surfaces the moment the recorder
//! catches up.
//!
//! New programs added in this round (per the recorder-test-requirements
//! universal checklist):
//!
//! | Program                       | Coverage                                    |
//! |-------------------------------|---------------------------------------------|
//! | `control_flow/ControlFlow.sol`| if/else + while + for, exact iteration count|
//! | `nested_calls/NestedCalls.sol`| 4-deep internal calls (run→outer→middle→inner)|
//! | `storage_ops/StorageOps.sol`  | SSTORE + SLOAD round-trip                   |
//! | `events_test/EventsTest.sol`  | three `emit` events surfaced as RecordEvent |
//! | `require_revert/RequireRevert.sol`| require/revert paths (with + without msg)|
//! | `map_struct_arr/MapStructArr.sol` | mapping + fixed-size array + struct     |

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------

/// Path to the `ct-print` binary shipped with `codetracer-trace-format-nim`.
fn ct_print_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("codetracer-trace-format-nim")
        .join("ct-print")
}

/// Helper: collect every `.ct` file in `out_dir`.
fn ct_files_in(out_dir: &Path) -> Vec<PathBuf> {
    if !out_dir.exists() {
        return Vec::new();
    }
    std::fs::read_dir(out_dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect()
}

fn has_solc() -> bool {
    Command::new("solc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn has_anvil() -> bool {
    Command::new("anvil")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skip-helper: returns `Some(path)` to ct-print or logs a clear
/// `SKIP:` diagnostic and returns `None`.
///
/// The `verify-cli-convention-no-silent-skip.sh` script greps for the
/// literal `SKIP:` token, so silent skips remain forbidden.
fn ct_print_or_skip(test_name: &str) -> Option<PathBuf> {
    if !has_solc() || !has_anvil() {
        eprintln!(
            "SKIP: {test_name} requires solc + anvil on PATH (use the Nix dev shell)."
        );
        return None;
    }
    let p = ct_print_path();
    if !p.exists() {
        eprintln!(
            "SKIP: {test_name} requires ct-print at {} — only available \
             within the metacraft workspace where codetracer-trace-format-nim \
             is a sibling.",
            p.display()
        );
        return None;
    }
    Some(p)
}

/// Path to a test program inside `test-programs/<group>/<name>.sol`.
fn test_program(group: &str, file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-programs")
        .join(group)
        .join(file)
}

/// Record `program` by invoking the recorder CLI's `record` subcommand
/// (the same path users invoke).  The CLI compiles via `solc`, deploys
/// to a transient anvil node, calls `function_name`, and writes a
/// `.ct` bundle into `out_dir`.
fn run_recorder_cli(program: &Path, out_dir: &Path, function_name: &str) {
    let bin = env!("CARGO_BIN_EXE_codetracer-evm-recorder");
    let output = Command::new(bin)
        .args(["record"])
        .arg(program)
        .args(["--out-dir"])
        .arg(out_dir)
        .args(["--function", function_name])
        .env_remove("CODETRACER_EVM_RECORDER_DISABLED")
        .env_remove("CODETRACER_EVM_RECORDER_OUT_DIR")
        .output()
        .expect("failed to run recorder");
    assert!(
        output.status.success(),
        "recorder CLI should succeed for {} :: {}; stdout: {}\nstderr: {}",
        program.display(),
        function_name,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Record one program and return the `ct-print --full --strip-paths`
/// JSON document.  Returns `None` when the prerequisites
/// (solc / anvil / ct-print) are absent — the caller has already
/// emitted a `SKIP:` line via `ct_print_or_skip`.
fn record_and_dump_full(
    test_name: &str,
    group: &str,
    file: &str,
    function_name: &str,
) -> Option<serde_json::Value> {
    let ct_print = ct_print_or_skip(test_name)?;

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = test_program(group, file);
    run_recorder_cli(&source_path, &out_dir, function_name);

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {out_dir:?}"
    );

    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");

    assert!(
        output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let doc: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("ct-print --full should emit valid JSON");

    drop(tmp_dir);
    Some(doc)
}

/// Decode the step events into `(step_index, line)` pairs in emission
/// order.  The recorder must emit step events with a strictly
/// increasing `step_index`; we assert that invariant here.
fn step_lines(doc: &serde_json::Value) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let mut last_idx = -1i64;
    for ev in doc["events"].as_array().expect("events array") {
        if ev["kind"] != "step" {
            continue;
        }
        let idx = ev["step_index"]
            .as_i64()
            .expect("step_index must be present on step events");
        assert!(
            idx > last_idx,
            "step_index must strictly increase; got {idx} after {last_idx}"
        );
        last_idx = idx;
        let line = ev["line"]
            .as_i64()
            .expect("line must be present on step events");
        out.push((idx, line));
    }
    out
}

/// Decode the line numbers of every step event, preserving emission order.
fn observed_step_lines(doc: &serde_json::Value) -> Vec<i64> {
    step_lines(doc).into_iter().map(|(_, l)| l).collect()
}

/// Decode the call-entry sequence as a vector of function names.
/// Function names that are absent (`function` is `null`) appear as the
/// empty string so the test can still assert ordering.
fn observed_call_entry_funcs(doc: &serde_json::Value) -> Vec<String> {
    doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .map(|e| {
            e["function"]
                .as_str()
                .unwrap_or("<unnamed>")
                .to_string()
        })
        .collect()
}

/// Decode the call-exit sequence as a vector of function names.
fn observed_call_exit_funcs(doc: &serde_json::Value) -> Vec<String> {
    doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            e["function"]
                .as_str()
                .unwrap_or("<unnamed>")
                .to_string()
        })
        .collect()
}

/// Decode every IO event as `(io_kind, text)` pairs.
fn observed_io_events(doc: &serde_json::Value) -> Vec<(String, String)> {
    doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "io")
        .map(|e| {
            let kind = e["io_kind"].as_str().unwrap_or("").to_string();
            let text = e["text"].as_str().unwrap_or("").to_string();
            (kind, text)
        })
        .collect()
}

/// Walk every `vars[]` entry in every `step` event and assert that
/// the *set of distinct* `value.kind`s observed is **exactly**
/// `expected`.
///
/// Strictness: any kind seen in the trace but absent from
/// `expected` is a hard error (the test author must extend the
/// per-test expectations); conversely, every kind in `expected`
/// MUST appear at least once in the trace, otherwise the test author
/// is asserting against a richer set than the recorder actually
/// emits and the assertion is silently weaker than intended.
///
/// See the recorder-test-requirements §1 "Maximum assertion
/// strength" — the per-program `_value_kinds_present` siblings rely
/// on this helper to surface the exact ValueRecord shape the
/// recorder produces today.
fn assert_step_value_kinds_eq(doc: &serde_json::Value, expected: &[&str]) {
    let mut found: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for ev in doc["events"].as_array().expect("events array") {
        if ev["kind"] != "step" {
            continue;
        }
        let Some(vars) = ev["vars"].as_array() else {
            continue;
        };
        for v in vars {
            let kind = v["value"]["kind"].as_str().unwrap_or("");
            found.insert(kind.to_string());
        }
    }
    let expected_set: std::collections::BTreeSet<String> =
        expected.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        found, expected_set,
        "step value kinds mismatch — extend the per-test expected set \
         rather than weakening the check (see \
         recorder-test-requirements §1)"
    );
}

/// Decode every observed `(varname, value_hex)` pair from step events,
/// in emission order.  Used to assert exact decoded values for storage
/// writes.  Stops at the first non-`Raw` value (asserts via the helper).
fn observed_step_var_pairs(doc: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for ev in doc["events"].as_array().expect("events array") {
        if ev["kind"] != "step" {
            continue;
        }
        let Some(vars) = ev["vars"].as_array() else {
            continue;
        };
        for v in vars {
            let kind = v["value"]["kind"].as_str().unwrap_or("");
            assert_eq!(
                kind, "Raw",
                "step var must decode as Raw; got {}",
                v["value"]
            );
            let name = v["varname"].as_str().unwrap_or("").to_string();
            let r = v["value"]["r"].as_str().unwrap_or("").to_string();
            out.push((name, r));
        }
    }
    out
}

/// Assert `metadata.program` is the canonical absolute path of the
/// recorded Solidity source file (per the cross-recorder convention
/// captured in `recorder-test-requirements.md` §1 — the EVM recorder
/// honours this convention as of the
/// `test_control_flow_metadata_program_is_source_path` fix).
///
/// We `assert_eq!` against the canonicalized `test-programs/<group>/<file>`
/// path computed exactly the same way the recorder CLI canonicalizes
/// its `solidity_file` argument, so the assertion is strict in the
/// `assert_eq!` sense but stays stable across checkouts.
fn assert_metadata_program_is_source_path(
    doc: &serde_json::Value,
    group: &str,
    file: &str,
) {
    let prog = doc["metadata"]["program"]
        .as_str()
        .expect("metadata.program str");
    let expected = test_program(group, file)
        .canonicalize()
        .expect("test program must exist and be canonicalizable");
    let expected_str = expected.to_string_lossy().to_string();
    assert_eq!(
        prog, expected_str,
        "metadata.program must be the canonical source-file path \
         (spec: recorder-test-requirements.md §1)"
    );
}

/// Assert `paths` contains exactly the source filename (under the
/// `<tmp>` strip-paths placeholder).
fn assert_paths_ends_with_source(doc: &serde_json::Value, source_filename: &str) {
    let paths: Vec<&str> = doc["paths"]
        .as_array()
        .expect("paths array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(paths.len(), 1, "expected exactly one source path; got {paths:?}");
    assert!(
        paths[0].ends_with(source_filename),
        "expected sole path entry to end with {source_filename}; got {}",
        paths[0]
    );
}

// ===========================================================================
// control_flow/ControlFlow.sol
// ===========================================================================

/// Records `ControlFlow.sol::run()` and asserts on the **exact** event
/// shape the recorder produces today.  The program exercises if/else
/// + while + for; the test pins the exact step-line sequence so any
/// change in step emission (e.g. dropping a loop iteration, dedup of
/// the loop-condition step) shows up as a hard test failure.
///
/// The recorder's current behaviour:
///
/// * `metadata.program` is the canonical absolute path of
///   `ControlFlow.sol` (per recorder-test-requirements.md §1).
/// * `functions` table contains only `fn_at_pc_<n>` placeholders —
///   none of the named Solidity functions land here because the
///   internal-call resolver only succeeds for argument-less calls
///   reachable through the AST.  RECORDER BUG.
/// * Every step variable is encoded as `ValueRecord::Raw` (a hex
///   string) — the recorder doesn't yet decode 256-bit stack words
///   to `ValueRecord::Int`.  RECORDER BUG, see the ignored sibling.
/// * The `Done(uint256)` event surfaces as a single `io` event with
///   `io_kind = "ioStderr"` (the multi-stream layout collapses
///   `EvmEvent` → `ioStderr`).  See `codetracer_trace_writer_ffi.nim`
///   `toIOEventKind`.
#[test]
fn test_control_flow_via_ct_print_full() {
    let Some(doc) = record_and_dump_full(
        "test_control_flow_via_ct_print_full",
        "control_flow",
        "ControlFlow.sol",
        "run",
    ) else {
        return;
    };

    assert_metadata_program_is_source_path(&doc, "control_flow", "ControlFlow.sol");
    assert_paths_ends_with_source(&doc, "ControlFlow.sol");

    // --- counts ---
    let counts = &doc["counts"];
    assert_eq!(counts["paths"].as_u64(), Some(1), "paths count");
    // 4 = `run` (registered when the dispatcher → entry-point JUMP is
    // absorbed) + 3 dispatcher-orphan `fn_at_pc_*` placeholders.
    assert_eq!(counts["functions"].as_u64(), Some(4), "functions count");
    assert_eq!(counts["steps"].as_u64(), Some(37), "steps count");
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls count");
    assert_eq!(counts["values"].as_u64(), Some(37), "values count");
    assert_eq!(counts["io_events"].as_u64(), Some(1), "io_events count");

    // --- exact step-line sequence ---
    // The shape encodes: dispatcher (1) → run() opening brace (19) →
    // the if/else assigns at lines 24-29 → while loop at lines
    // 35-39 (cond at 37, body at 38-39, repeated 3 times) → for
    // loop at lines 44-46 (init+cond at 45, body at 46, 5 iterations
    // — body line ordering: 45→46 ×5 then a final 45 for the
    // exit-condition check) → tail at 50-53 → return-site step at
    // line 24.
    //
    // RECORDER BUG: a spec-compliant trace would emit a separate step
    // for *every* iteration of the for-loop body (5 events at line
    // 46 — observed) but the loop-condition step at line 45 fires 6
    // times (init + 5 increments).  Any deviation from this pinned
    // pattern is a real recorder regression.
    let lines = observed_step_lines(&doc);
    assert_eq!(
        lines,
        vec![
            1,                     // dispatcher entry
            19,                    // contract opener
            24,                    // function run() {
            26, 27, 28, 29,        // bool flag = true; uint256 branchVal; if(flag) {
            28,                    //   } (post-if)
            35, 36,                // uint256 whileSum = 0; uint256 counter = 0;
            37, 38, 39,            // while-loop iteration #1 (cond, body, body)
            37, 38, 39,            // while-loop iteration #2
            37, 38, 39,            // while-loop iteration #3
            37,                    // while-loop final cond (false)
            44, 45, 46,            // uint256 forSum = 0; for init+cond; body
            45, 46,                // for-loop iter #2
            45, 46,                // for-loop iter #3
            45, 46,                // for-loop iter #4
            45, 46,                // for-loop iter #5
            45,                    // for-loop final cond (i==6, false)
            50, 51, 52, 53,        // tail: total = ...; result = total; emit; return
            24,                    // return-site step at function header
        ]
    );

    // --- value variants ---
    // Locals (`flag`, `branchVal`, `whileSum`, `counter`, `forSum`,
    // `i`, `total`) are u256 stack values that fit in i64 → emitted
    // as `ValueRecord::Int`.  Storage `result` and the carried-
    // forward storage cache stay `ValueRecord::Raw` (keeps the
    // `observed_step_var_pairs` storage-hex check honest).
    assert_step_value_kinds_eq(&doc, &["Int", "Raw"]);

    // --- io / event emission ---
    // Exactly one Done(uint256) event → one ioStderr line in the
    // multi-stream IO channel.
    let ios = observed_io_events(&doc);
    assert_eq!(ios.len(), 1, "expected exactly one io event for emit Done(...)");
    assert_eq!(ios[0].0, "ioStderr", "io_kind for EvmEvent collapses to ioStderr");
    // The text payload is the topic0 (keccak256("Done(uint256)")) hex.
    assert!(
        ios[0].1.starts_with("0x") && ios[0].1.len() == 66,
        "Done event topic0 must be a 32-byte hex string; got {}",
        ios[0].1
    );

    // --- call sequence ---
    // RECORDER BUG: the trace closes with two orphan call_entry +
    // call_exit pairs emitted *after* the user function returns,
    // both with `function: null` in the JSON output (so they
    // surface as `<unnamed>` via `observed_call_entry_funcs`).
    // These are emitted by the recorder's dispatcher post-return
    // walking and don't correspond to any user-visible Solidity
    // function.  Pinned here so any change in call-event emission
    // is caught.
    assert_eq!(
        observed_call_entry_funcs(&doc),
        vec!["<unnamed>".to_string(), "<unnamed>".to_string()],
        "ControlFlow.run() emits two orphan dispatcher call_entries; \
         RECORDER BUG: spec wants `run` here"
    );
    assert_eq!(
        observed_call_exit_funcs(&doc),
        vec!["<unnamed>".to_string(), "<unnamed>".to_string()],
    );
    let counts_calls = &doc["counts"]["calls"];
    assert_eq!(counts_calls.as_u64(), Some(2), "calls count is the orphan pair");
}

#[test]
fn test_control_flow_metadata_program_is_source_path() {
    let Some(doc) = record_and_dump_full(
        "test_control_flow_metadata_program_is_source_path",
        "control_flow",
        "ControlFlow.sol",
        "run",
    ) else {
        return;
    };
    let prog = doc["metadata"]["program"].as_str().unwrap_or("");
    assert!(
        prog.ends_with("ControlFlow.sol"),
        "metadata.program should end with the source filename; got {prog}"
    );
}

#[test]
fn test_control_flow_decodes_loop_sums() {
    let Some(doc) = record_and_dump_full(
        "test_control_flow_decodes_loop_sums",
        "control_flow",
        "ControlFlow.sol",
        "run",
    ) else {
        return;
    };
    // When the recorder decodes uint256 → Int we expect to find
    // (whileSum, 3), (forSum, 15), (total, 118) in the step value
    // stream.
    let mut found_int = false;
    for ev in doc["events"].as_array().unwrap() {
        if ev["kind"] != "step" {
            continue;
        }
        for v in ev["vars"].as_array().cloned().unwrap_or_default() {
            if v["value"]["kind"] == "Int" {
                found_int = true;
            }
        }
    }
    assert!(found_int, "expected at least one ValueRecord::Int variant");
}

// ===========================================================================
// nested_calls/NestedCalls.sol
// ===========================================================================

/// Records `NestedCalls.sol::run()` and asserts on the **exact** call
/// nesting and step-line shape.  The intended chain is
/// `run → outer → middle → inner` (4 deep), but the recorder absorbs
/// the dispatcher-into-`run` jump into `<toplevel>` (see the
/// `first_internal_call_absorbed` logic in `recorder.rs`), so
/// `run` is not re-emitted as a call_entry.  The visible chain is
/// therefore 3 deep: `outer → middle → inner`.
#[test]
fn test_nested_calls_via_ct_print_full() {
    let Some(doc) = record_and_dump_full(
        "test_nested_calls_via_ct_print_full",
        "nested_calls",
        "NestedCalls.sol",
        "run",
    ) else {
        return;
    };

    assert_metadata_program_is_source_path(&doc, "nested_calls", "NestedCalls.sol");
    assert_paths_ends_with_source(&doc, "NestedCalls.sol");

    // --- counts ---
    let counts = &doc["counts"];
    assert_eq!(counts["paths"].as_u64(), Some(1), "paths count");
    // 6 = `run` (registered when the dispatcher → entry-point JUMP is
    // absorbed) + the 3 AST-resolved internal calls (`outer`,
    // `middle`, `inner`) + 2 dispatcher-orphan `fn_at_pc_*`
    // placeholders.
    assert_eq!(counts["functions"].as_u64(), Some(6), "functions count");
    assert_eq!(counts["steps"].as_u64(), Some(26), "steps count");
    // 3 internal-call entries that surface in the call_entry
    // sequence + 2 more orphan entries from the dispatcher
    // post-return — see the assertions further down.
    assert_eq!(counts["calls"].as_u64(), Some(5), "calls count");
    assert_eq!(counts["io_events"].as_u64(), Some(1), "io_events count");

    // --- function table ---
    // `run` lands first because the recorder registers it eagerly
    // when it absorbs the dispatcher → entry-point JUMP into
    // <toplevel> (so step-over works).  The next three are
    // AST-resolved internals (`outer`, `middle`, `inner`); the last
    // two `fn_at_pc_*` entries are dispatcher-orphan placeholders.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        functions,
        vec!["run", "outer", "middle", "inner", "fn_at_pc_410", "fn_at_pc_340"],
        "function table — order is writer-assignment order; \
         entry-point first, then AST-resolved internals, then \
         lookahead-fallback placeholders"
    );

    // --- exact step-line sequence ---
    // Lines 17-22 are run() body.  17/18/19 are the local
    // declarations + the call to outer().  Lines 26-28 are outer()
    // body.  Lines 31-33 are middle() body.  Lines 36-39 are
    // inner() body.  After the leaf returns we walk back up through
    // 36 → 32 → 31 → 27 → 28 → 26 → 19 → 20 → 21 → 22 → 23 → 17.
    let lines = observed_step_lines(&doc);
    assert_eq!(
        lines,
        vec![
            1, 12, 17, 18, 19,        // dispatcher → contract → run() → seed → call outer
            26, 27,                   // outer() opener + call middle
            31, 32,                   // middle() opener + call inner
            36, 37, 38,               // inner() opener + a/b decls
            39,                       // return a + b
            36, 32, 33,               // unwind through inner→middle (return i + 10)
            31, 27, 28,               // unwind through middle→outer (return m + 100)
            26, 19, 20, 21, 22, 23,   // unwind to run(): r = seed + v; stored = r; emit; return
            17,                       // return-site step
        ],
        "step-line sequence pins the run→outer→middle→inner unwind"
    );

    // --- value variants ---
    // Locals (`seed`, `v`, `r`, `m`, `i`, `a`, `b`) and the storage
    // `stored` carry-forward all surface in step events: locals as
    // `Int` (small u256 values that fit in i64), storage carry-
    // forward as `Raw`.
    assert_step_value_kinds_eq(&doc, &["Int", "Raw"]);

    // --- io / event emission ---
    let ios = observed_io_events(&doc);
    assert_eq!(ios.len(), 1, "expected one Result(uint256) event");
    assert_eq!(ios[0].0, "ioStderr");

    // --- call entry/exit sequence ---
    // The first three call_entries are the AST-resolved internals
    // (outer, middle); the third's name is null because `inner` is
    // not registered in the function table (see RECORDER BUG above).
    // The fourth + fifth are the orphan dispatcher calls at the end.
    let entries = observed_call_entry_funcs(&doc);
    assert_eq!(
        entries,
        vec![
            "fn_at_pc_410".to_string(), // outer
            "fn_at_pc_340".to_string(), // middle
            "<unnamed>".to_string(),    // inner — name not resolved
            "<unnamed>".to_string(),    // dispatcher orphan #1
            "<unnamed>".to_string(),    // dispatcher orphan #2
        ]
    );

    // call_exit sequence is symmetric: inner → middle → outer
    // (LIFO), then the two orphan exits.
    let exits = observed_call_exit_funcs(&doc);
    assert_eq!(
        exits,
        vec![
            "fn_at_pc_410".to_string(),
            "fn_at_pc_340".to_string(),
            "<unnamed>".to_string(),
            "<unnamed>".to_string(),
            "<unnamed>".to_string(),
        ]
    );
}

#[test]
fn test_nested_calls_function_names_resolved() {
    let Some(doc) = record_and_dump_full(
        "test_nested_calls_function_names_resolved",
        "nested_calls",
        "NestedCalls.sol",
        "run",
    ) else {
        return;
    };
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for name in ["run", "outer", "middle", "inner"] {
        assert!(
            functions.contains(&name),
            "function table should include `{name}`; got {functions:?}"
        );
    }
}

// ===========================================================================
// storage_ops/StorageOps.sol
// ===========================================================================

/// Records `StorageOps.sol::run()` — three SSTOREs followed by three
/// SLOADs.  The trace must surface the storage variables (`a`, `b`,
/// `c`) and the local read-back variables (`ra`, `rb`, `rc`).
#[test]
fn test_storage_ops_via_ct_print_full() {
    let Some(doc) = record_and_dump_full(
        "test_storage_ops_via_ct_print_full",
        "storage_ops",
        "StorageOps.sol",
        "run",
    ) else {
        return;
    };

    assert_metadata_program_is_source_path(&doc, "storage_ops", "StorageOps.sol");
    assert_paths_ends_with_source(&doc, "StorageOps.sol");

    // --- counts ---
    let counts = &doc["counts"];
    assert_eq!(counts["paths"].as_u64(), Some(1), "paths count");
    // 4 = `run` (eagerly registered for the absorbed entry-point JUMP)
    // + 3 dispatcher-orphan `fn_at_pc_*` placeholders.
    assert_eq!(counts["functions"].as_u64(), Some(4), "functions count");
    assert_eq!(counts["steps"].as_u64(), Some(12), "steps count");
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls count");
    assert_eq!(counts["io_events"].as_u64(), Some(1), "io_events count");

    // --- varnames ---
    // The recorder must register every storage and local variable
    // name it surfaces.  Order is registration (first-touch) order.
    let varnames: Vec<&str> = doc["varnames"]
        .as_array()
        .expect("varnames array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        varnames,
        vec!["a", "b", "c", "ra", "rb", "rc"],
        "varnames table includes all three storage slots + three locals"
    );

    // --- exact step-line sequence ---
    let lines = observed_step_lines(&doc);
    assert_eq!(
        lines,
        vec![
            1, 11,                 // dispatcher + contract
            18,                    // function run() {
            20, 21, 22,            // a = 10; b = 20; c = 30;
            25, 26, 27,            // uint256 ra = a; rb = b; rc = c;
            29, 30,                // emit + return
            18,                    // return-site
        ],
    );

    // --- value variants ---
    // Locals (`ra`, `rb`, `rc`) and the storage carry-forward of
    // `a`, `b`, `c` all surface in step events: locals as `Int`
    // (small u256 values), storage carry-forward as `Raw`.
    assert_step_value_kinds_eq(&doc, &["Int", "Raw"]);

    // --- decoded storage values ---
    // After the three SSTOREs the storage variables `a`, `b`, `c`
    // must surface with the literal hex values 0xa, 0x14, 0x1e.
    // We collect every (varname, value) pair across all step events
    // and assert that the final write of each storage slot matches
    // the source-program literal.
    let pairs = observed_step_var_pairs(&doc);
    let last_a = pairs.iter().rev().find(|(n, _)| n == "a").map(|(_, v)| v.as_str());
    let last_b = pairs.iter().rev().find(|(n, _)| n == "b").map(|(_, v)| v.as_str());
    let last_c = pairs.iter().rev().find(|(n, _)| n == "c").map(|(_, v)| v.as_str());
    assert_eq!(last_a, Some("0xa"), "storage a must end at 10 (0xa)");
    assert_eq!(last_b, Some("0x14"), "storage b must end at 20 (0x14)");
    assert_eq!(last_c, Some("0x1e"), "storage c must end at 30 (0x1e)");

    // --- io ---
    let ios = observed_io_events(&doc);
    assert_eq!(ios.len(), 1, "Stored(uint256,uint256,uint256) → 1 LOG opcode");
    assert_eq!(ios[0].0, "ioStderr");
}

// ===========================================================================
// events_test/EventsTest.sol
// ===========================================================================

/// Records `EventsTest.sol::run()` which fires three distinct `emit`
/// statements.  Each must surface as exactly one `RecordEvent` (which
/// the multi-stream layout collapses into an `ioStderr` IO event).
#[test]
fn test_events_test_via_ct_print_full() {
    let Some(doc) = record_and_dump_full(
        "test_events_test_via_ct_print_full",
        "events_test",
        "EventsTest.sol",
        "run",
    ) else {
        return;
    };

    assert_metadata_program_is_source_path(&doc, "events_test", "EventsTest.sol");
    assert_paths_ends_with_source(&doc, "EventsTest.sol");

    // --- counts ---
    let counts = &doc["counts"];
    assert_eq!(counts["paths"].as_u64(), Some(1), "paths count");
    // 3 = `run` (eagerly registered for the absorbed entry-point JUMP)
    // + 2 dispatcher-orphan `fn_at_pc_*` placeholders.
    assert_eq!(counts["functions"].as_u64(), Some(3), "functions count");
    assert_eq!(counts["steps"].as_u64(), Some(9), "steps count");
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls count");
    // EXACT three io_events — one per `emit` statement.  This is
    // the headline assertion for this program: an off-by-one in the
    // recorder's LOG-opcode handling would surface here.
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(3),
        "expected exactly three io_events (one per emit); counts={counts}"
    );

    // --- exact step-line sequence ---
    let lines = observed_step_lines(&doc);
    assert_eq!(
        lines,
        vec![
            1, 12,                 // dispatcher + contract
            19,                    // function run() {
            20, 21, 22, 23, 24,    // emit Started; emit Tagged(7); emit Payload(7,42); stored = 42; return 42
            19,                    // return-site
        ],
    );

    // --- value variants ---
    // EventsTest.run() declares no parameters or named locals, so
    // the only values surfaced in step events are the storage
    // carry-forward of `stored` (Raw u256).
    assert_step_value_kinds_eq(&doc, &["Raw"]);

    // --- io events ---
    let ios = observed_io_events(&doc);
    assert_eq!(ios.len(), 3, "exactly three io events for the three emits");

    // io[0] = emit Started()             → LOG1, just topic0
    // io[1] = emit Tagged(7)             → LOG2, topic0 + topic1 (indexed key=7)
    // io[2] = emit Payload(7, 42)        → LOG2, topic0 + topic1 (indexed key=7)
    //
    // The recorder serialises topics as a comma-separated hex list;
    // the non-indexed `value=42` payload from Payload is read from
    // EVM memory and is not surfaced — see the
    // `_io_payload_includes_topics_and_data` ignored sibling.
    assert_eq!(ios[0].0, "ioStderr");
    assert!(
        ios[0].1.starts_with("0x") && ios[0].1.len() == 66,
        "Started: only topic0; got {}",
        ios[0].1
    );

    assert_eq!(ios[1].0, "ioStderr");
    assert!(
        ios[1].1.contains(", 0x7"),
        "Tagged(7) must include indexed arg `7` as topic1; got {}",
        ios[1].1
    );

    assert_eq!(ios[2].0, "ioStderr");
    assert!(
        ios[2].1.contains(", 0x7"),
        "Payload(7, 42) must include indexed arg `7` as topic1; got {}",
        ios[2].1
    );
}

#[test]
fn test_events_test_io_payload_includes_topics_and_data() {
    let Some(doc) = record_and_dump_full(
        "test_events_test_io_payload_includes_topics_and_data",
        "events_test",
        "EventsTest.sol",
        "run",
    ) else {
        return;
    };
    let ios = observed_io_events(&doc);
    // Tagged(uint256) has indexed arg `7`; Payload(uint256,uint256)
    // has indexed arg `7` plus non-indexed data `42`.  The
    // serialised form must include both.
    assert!(
        ios.iter().any(|(_, text)| text.contains(",")),
        "expected at least one io payload to carry multiple comma-separated topics"
    );
}

// ===========================================================================
// require_revert/RequireRevert.sol
// ===========================================================================

/// Records the **happy path** `RequireRevert.sol::run()` (which
/// invokes the `safe(true)` internal call — the require passes,
/// storage is written, an event is emitted).  This pins the trace
/// the recorder produces for a `require`-protected internal call.
///
/// The failing paths (`failingRequire`, `failingRevert`) are exercised
/// via the `_revert_path_emits_error_event` ignored sibling — they
/// currently can't be recorded through the CLI (the recorder fails
/// the transaction at `provider.send_transaction` because alloy
/// errors on revert before `debug_traceTransaction` can be fetched).
#[test]
fn test_require_revert_happy_path_via_ct_print_full() {
    let Some(doc) = record_and_dump_full(
        "test_require_revert_happy_path_via_ct_print_full",
        "require_revert",
        "RequireRevert.sol",
        "run",
    ) else {
        return;
    };

    assert_metadata_program_is_source_path(&doc, "require_revert", "RequireRevert.sol");
    assert_paths_ends_with_source(&doc, "RequireRevert.sol");

    // --- counts ---
    let counts = &doc["counts"];
    assert_eq!(counts["paths"].as_u64(), Some(1), "paths count");
    // 3 = `run` (eagerly registered for the absorbed entry-point JUMP)
    // + `safe` + 1 dispatcher-orphan `fn_at_pc_*` placeholder.
    assert_eq!(counts["functions"].as_u64(), Some(3), "functions count");
    assert_eq!(counts["steps"].as_u64(), Some(13), "steps count");
    assert_eq!(counts["calls"].as_u64(), Some(3), "calls count");
    assert_eq!(counts["io_events"].as_u64(), Some(1), "io_events count");

    // --- function table ---
    // `run` is registered first (the recorder eagerly registers the
    // entry-point name when it absorbs the dispatcher → `run` JUMP
    // into <toplevel>).  `safe` is AST-resolved (single bool
    // argument; the recorder's resolver succeeds for it because the
    // lookahead window catches the body offset).  The trailing entry
    // is a placeholder for the dispatcher-orphan call.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        functions,
        vec!["run", "safe", "fn_at_pc_458"],
        "function table — entry-point `run` first, then `safe` resolved by AST, then dispatcher orphan placeholder"
    );

    // --- exact step-line sequence ---
    // run() body is lines 18-22.  The internal call to safe(bool)
    // jumps to lines 25-27 (require + return).  The require holds
    // so the require expression at 26 is the only branch traversed.
    let lines = observed_step_lines(&doc);
    assert_eq!(
        lines,
        vec![
            1, 13,                  // dispatcher + contract
            18, 19,                 // function run() { ... uint256 v = safe(true);
            25, 26, 27, 25,         // safe(): require(flag,...); return 7; ret-site
            19, 20, 21, 22,         // back in run(): stored = v; emit; return v
            18,                     // return-site
        ],
    );

    // --- value variants ---
    // The local `v` (uint256, value 7) and the `safe(bool flag)`
    // parameter `flag` (bool, value 1) both surface as `Int` (small
    // u256 values fitting in i64); the storage carry-forward of
    // `stored` stays `Raw`.
    assert_step_value_kinds_eq(&doc, &["Int", "Raw"]);

    // --- io ---
    let ios = observed_io_events(&doc);
    assert_eq!(ios.len(), 1, "happy path emits Ok(uint256) once");
    assert_eq!(ios[0].0, "ioStderr");

    // --- call sequence ---
    // safe() is the only resolved internal call; the other two are
    // dispatcher orphans.
    let entries = observed_call_entry_funcs(&doc);
    assert_eq!(
        entries,
        vec![
            "safe".to_string(),
            "fn_at_pc_458".to_string(),
            "fn_at_pc_458".to_string(),
        ]
    );
}

#[test]
#[ignore = "RECORDER BUG: failing transactions (revert / require fail) cannot \
            be recorded today — the alloy provider raises before \
            `debug_traceTransaction` is fetched, so the recorder CLI exits \
            non-zero with no trace produced.  Spec wants the recorder to \
            still capture the structlog up to the REVERT opcode and surface \
            an EventLogKind::Error io event carrying the revert reason."]
fn test_require_revert_failing_path_emits_error_event() {
    // This test is intentionally incomplete: the path to record a
    // reverted transaction needs a provider that doesn't error on
    // revert.  Tracking the spec-correct expectation: the recorder
    // should produce a .ct bundle containing at least one io event
    // of kind `ioError` whose text includes the revert reason
    // ("always fails").
    let Some(_) = record_and_dump_full(
        "test_require_revert_failing_path_emits_error_event",
        "require_revert",
        "RequireRevert.sol",
        "failingRequire",
    ) else {
        return;
    };
    panic!("recorder should produce a trace for a reverting tx");
}

// ===========================================================================
// map_struct_arr/MapStructArr.sol
// ===========================================================================

/// Records `MapStructArr.sol::run()` which writes a mapping entry,
/// fills a fixed-size array, and assigns a struct.  This pins the
/// current shape; the spec-correct expectation (mappings, arrays,
/// structs surface as `ValueRecord::Sequence`/`Struct`) lives in the
/// `_value_kinds_present` ignored sibling.
#[test]
fn test_map_struct_arr_via_ct_print_full() {
    let Some(doc) = record_and_dump_full(
        "test_map_struct_arr_via_ct_print_full",
        "map_struct_arr",
        "MapStructArr.sol",
        "run",
    ) else {
        return;
    };

    assert_metadata_program_is_source_path(&doc, "map_struct_arr", "MapStructArr.sol");
    assert_paths_ends_with_source(&doc, "MapStructArr.sol");

    // --- counts ---
    let counts = &doc["counts"];
    assert_eq!(counts["paths"].as_u64(), Some(1), "paths count");
    // 3 = `run` (eagerly registered for the absorbed entry-point JUMP)
    // + 2 dispatcher-orphan `fn_at_pc_*` placeholders.
    assert_eq!(counts["functions"].as_u64(), Some(3), "functions count");
    assert_eq!(counts["steps"].as_u64(), Some(11), "steps count");
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls count");
    assert_eq!(counts["io_events"].as_u64(), Some(1), "io_events count");

    // --- varnames ---
    // RECORDER BUG: the mapping write surfaces under a synthetic
    // `storage[<huge keccak slot>]` name (the slot key for
    // `balances[msg.sender]`); the struct fields beyond the layout
    // table surface as `storage[5]` / `storage[6]`.  Spec wants
    // these to be resolved against the storage layout to
    // `balances[msg.sender]` and `record.value` / `record.active`.
    let varnames: Vec<&str> = doc["varnames"]
        .as_array()
        .expect("varnames array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    // The first entry is the synthetic mapping storage slot — its
    // exact decimal varies per anvil deployment because it is the
    // keccak256 of the deployer address concatenated with slot 0.
    // We assert on shape, not exact value.
    assert!(
        varnames[0].starts_with("storage["),
        "expected synthetic mapping varname; got {}",
        varnames[0]
    );
    assert_eq!(
        &varnames[1..],
        &[
            "slots",
            "storage[2]",
            "storage[3]",
            "record",
            "storage[5]",
            "storage[6]",
        ],
    );

    // --- exact step-line sequence ---
    let lines = observed_step_lines(&doc);
    assert_eq!(
        lines,
        vec![
            1, 18,                  // dispatcher + contract
            31, 32,                 // function run() { balances[msg.sender] = 100;
            34, 35, 36,             // slots[0..2] = 1, 2, 3;
            38,                     // record = Record({...});
            40, 41,                 // emit Done(); return slots[0]+slots[1]+slots[2]+record.value
            31,                     // return-site
        ],
    );

    // --- value variants ---
    // - `Raw` for individual storage slot writes (the synthetic
    //   `storage[<slot>]` entries) and the storage carry-forward.
    // - `Sequence` for the `slots` fixed-size array snapshot
    //   (assembled from layout-resolved slots 1..4).
    // - `Struct` for the `record` struct snapshot (assembled from
    //   layout-resolved member slots 4..7).
    //
    // run() returns the sum `slots[0]+slots[1]+slots[2]+record.value`
    // but does not bind it to a named local, so no `Int` kind appears.
    assert_step_value_kinds_eq(&doc, &["Raw", "Sequence", "Struct"]);

    // --- io ---
    let ios = observed_io_events(&doc);
    assert_eq!(ios.len(), 1, "expected one Done() event");
    assert_eq!(ios[0].0, "ioStderr");
}

#[test]
fn test_map_struct_arr_value_kinds_present() {
    let Some(doc) = record_and_dump_full(
        "test_map_struct_arr_value_kinds_present",
        "map_struct_arr",
        "MapStructArr.sol",
        "run",
    ) else {
        return;
    };
    let mut kinds = std::collections::BTreeSet::new();
    for ev in doc["events"].as_array().unwrap() {
        if ev["kind"] != "step" {
            continue;
        }
        for v in ev["vars"].as_array().cloned().unwrap_or_default() {
            if let Some(k) = v["value"]["kind"].as_str() {
                kinds.insert(k.to_string());
            }
        }
    }
    for want in ["Sequence", "Struct"] {
        assert!(
            kinds.contains(want),
            "expected {want} ValueRecord variant in MapStructArr trace; got {kinds:?}"
        );
    }
}
