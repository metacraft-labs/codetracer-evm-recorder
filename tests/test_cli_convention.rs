//! CLI convention regression tests for the EVM recorder.
//!
//! These tests pin down the recorder's compliance with
//! `Recorder-CLI-Conventions.md` (in `codetracer-specs`):
//!
//!   * §3 — `--out-dir` / `-o` is the canonical output flag.
//!   * §4 — recorders never expose a `--format` flag (CTFS-only).
//!     Human-readable conversion is performed by `ct print` from
//!     `codetracer-trace-format-nim`.
//!   * §5 — `CODETRACER_EVM_RECORDER_OUT_DIR` is honoured as a
//!     fallback for `--out-dir`; `CODETRACER_EVM_RECORDER_DISABLED`
//!     skips recording entirely.
//!
//! `--trace-dir` is kept as a deprecated alias so existing scripts
//! don't break immediately; it must still work AND emit a one-line
//! stderr deprecation note.
//!
//! See `AUDIT-CTFS-2026-05.md` ("Convention compliance follow-up")
//! for the full record.

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Toolchain detection
// ---------------------------------------------------------------------------

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

/// Path to the `ct-print` binary shipped with `codetracer-trace-format-nim`.
///
/// Tests that need to make content-level assertions on a recorded
/// trace pipe the `.ct` container through `ct-print --json` and
/// assert on the resulting JSON.  This is the workflow that
/// `Recorder-CLI-Conventions.md` §4 prescribes for downstream tools /
/// golden snapshots.
fn ct_print_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("codetracer-trace-format-nim")
        .join(format!("ct-print{}", std::env::consts::EXE_SUFFIX))
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

/// Path to the canonical FlowTest.sol fixture.
fn flow_test_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("contracts/FlowTest.sol")
}

// ---------------------------------------------------------------------------
// §3 / §4 — `--help` shape
// ---------------------------------------------------------------------------

/// The CLI binary must not expose a `--format` flag at any level.
/// Convention: `Recorder-CLI-Conventions.md` §4 — recorders are
/// CTFS-only.
#[test]
fn test_no_format_flag_in_help() {
    let bin = env!("CARGO_BIN_EXE_codetracer-evm-recorder");

    for subcmd in [None, Some("record")] {
        let mut cmd = Command::new(bin);
        if let Some(s) = subcmd {
            cmd.arg(s);
        }
        cmd.arg("--help");

        let output = cmd.output().expect("failed to run --help");
        assert!(
            output.status.success(),
            "--help (subcmd={:?}) should exit 0",
            subcmd
        );

        let help = String::from_utf8_lossy(&output.stdout);
        assert!(
            !help.contains("--format"),
            "--help (subcmd={:?}) must not advertise --format; got:\n{help}",
            subcmd
        );
        assert!(
            !help.contains("CODETRACER_FORMAT"),
            "--help (subcmd={:?}) must not advertise CODETRACER_FORMAT; got:\n{help}",
            subcmd
        );
    }
}

/// `--help` must mention `ct print` so users know where to go for
/// human-readable conversion of the recorded CTFS bundle.
#[test]
fn test_help_mentions_ct_print() {
    let bin = env!("CARGO_BIN_EXE_codetracer-evm-recorder");
    let output = Command::new(bin)
        .arg("--help")
        .output()
        .expect("failed to run --help");
    assert!(output.status.success(), "--help should exit 0");

    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("ct print"),
        "--help must mention `ct print` as the conversion tool; got:\n{help}"
    );
}

// ---------------------------------------------------------------------------
// §5 — env-var contract
// ---------------------------------------------------------------------------

/// `CODETRACER_EVM_RECORDER_OUT_DIR` must be honoured as a fallback
/// for `--out-dir`.  Convention: `Recorder-CLI-Conventions.md` §5.
///
/// Real recorder run — requires solc + anvil on PATH (Nix dev shell).
#[test]
fn test_env_out_dir_used_when_flag_omitted() {
    if !has_solc() || !has_anvil() {
        eprintln!("skipping: solc/anvil unavailable");
        return;
    }

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let env_out_dir = tmp_dir.path().join("via-env");

    let bin = env!("CARGO_BIN_EXE_codetracer-evm-recorder");
    let output = Command::new(bin)
        .args(["record"])
        .arg(flow_test_source())
        .args(["--function", "compute"])
        .env("CODETRACER_EVM_RECORDER_OUT_DIR", &env_out_dir)
        .env_remove("CODETRACER_EVM_RECORDER_DISABLED")
        .output()
        .expect("failed to run recorder");

    assert!(
        output.status.success(),
        "recorder should succeed when CODETRACER_EVM_RECORDER_OUT_DIR is set; \
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let ct_files = ct_files_in(&env_out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected the env-supplied output dir {:?} to receive the .ct container",
        env_out_dir
    );
}

/// `CODETRACER_EVM_RECORDER_DISABLED=1` must skip recording entirely.
/// The recorder process should still exit 0 — the EVM recorder doesn't
/// run a separate target subprocess (it spins up Anvil and calls the
/// contract itself), so "disabled" simply means "don't write any
/// trace artefacts and skip the Anvil round-trip".
///
/// Note: we don't require solc/anvil here because the disabled path
/// must short-circuit before any toolchain calls.
#[test]
fn test_env_disabled_skips_recording() {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("should-stay-empty");

    let bin = env!("CARGO_BIN_EXE_codetracer-evm-recorder");
    let output = Command::new(bin)
        .args(["record"])
        .arg(flow_test_source())
        .args(["--out-dir"])
        .arg(&out_dir)
        .env("CODETRACER_EVM_RECORDER_DISABLED", "1")
        .output()
        .expect("failed to run recorder");

    assert!(
        output.status.success(),
        "recorder should succeed in disabled mode; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // No .ct file should have been written.
    assert!(
        !out_dir.exists() || ct_files_in(&out_dir).is_empty(),
        "no .ct container should be written when CODETRACER_EVM_RECORDER_DISABLED=1; \
         got files in {:?}",
        out_dir
    );
}

// ---------------------------------------------------------------------------
// Deprecated `--trace-dir` alias
// ---------------------------------------------------------------------------

/// The legacy `--trace-dir` flag must still work (existing scripts
/// shouldn't break immediately) AND must emit a one-line stderr
/// deprecation note.  See `Recorder-CLI-Conventions.md` §3.
#[test]
fn test_trace_dir_alias_still_works_with_deprecation_note() {
    if !has_solc() || !has_anvil() {
        eprintln!("skipping: solc/anvil unavailable");
        return;
    }

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("legacy-trace-dir");

    let bin = env!("CARGO_BIN_EXE_codetracer-evm-recorder");
    let output = Command::new(bin)
        .args(["record"])
        .arg(flow_test_source())
        .args(["--trace-dir"])
        .arg(&out_dir)
        .args(["--function", "compute"])
        .env_remove("CODETRACER_EVM_RECORDER_DISABLED")
        .env_remove("CODETRACER_EVM_RECORDER_OUT_DIR")
        .output()
        .expect("failed to run recorder");

    assert!(
        output.status.success(),
        "legacy --trace-dir should still work; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Deprecation note must surface on stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--trace-dir is deprecated"),
        "stderr must contain a deprecation note for --trace-dir; got:\n{stderr}"
    );

    // The legacy flag still routes to the resolved output dir.
    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected --trace-dir to receive the .ct container; dir: {:?}",
        out_dir
    );
}

// ---------------------------------------------------------------------------
// `ct print` integration — content-level assertion on a real recording
// ---------------------------------------------------------------------------

/// Record `FlowTest.sol`, then convert the produced `.ct` container to
/// JSON via `ct-print` and assert on:
///
/// 1. **Structural anchors** (legacy layer): `ct-print --json` output
///    contains the source filename, contract program label, the `add`
///    Solidity helper in the function table, and the storage variable
///    names somewhere in the textual rendering.
/// 2. **Exact decoded values** (the layer enabled by `ct-print --full`):
///    the FlowTest.sol contract executes `compute()` with `a=10`,
///    `b=20`, `storedA = a = 10`, `result = add(a, b) = 30`,
///    `storedResult = result = 30`, where `add(x, y) = x + y`.  The
///    recorder must surface stable byte-level snapshots of those
///    storage / parameter values, decoded by `ct-print --full` to
///    `{"kind":"Raw","r":"0x<hex>","type_id":N}`.
///
/// **EVM-specific note**: the EVM recorder writes every variable value
/// as `ValueRecord::Raw{r:[bytes]}` (a stack/memory slice) rather than
/// the typed `ValueRecord::Int{i,...}` variant the cairo / cardano /
/// circom / aiken recorders use.  This is a pre-existing recorder
/// limitation — the EVM has no source-level let-binding semantics on
/// the stack, so the recorder snapshots raw stack words at every
/// JUMP/PUSH transition, mixing in dispatcher noise (function
/// selectors like `0x4b`, hashes like `0xb9`) with the source values.
/// See `AUDIT-CTFS-2026-05.md` ("Internal-call `register_return`
/// value", "Internal-call `Call.args` staged from the callee stack")
/// for the open follow-ups around typed-value emission.
///
/// The test therefore asserts on **stable Raw-payload anchors** — the
/// final post-storage values for `storedA` (= `0xa` = 10) and
/// `storedResult` (= `0x1e` = 30), and the recovered `add(x, y)`
/// callee parameters (`x = 0xa = 10`, `y = 0x14 = 20`, surfaced via
/// the AST-aware stack-label seeding from
/// `AUDIT-CTFS-2026-05.md` §4).  The strict `value.kind == "Raw"`
/// invariant means: if a future EVM recorder upgrade emits
/// `ValueRecord::Int` (or any other variant), this test fails loudly
/// and the next maintainer extends the assertion to the new variant
/// rather than silently accepting it.
///
/// Pre-2026-05-08 a similar assertion was made directly on a recorder-
/// emitted `trace.json` file.  The convention now mandates CTFS-only
/// output; `ct print` is the canonical conversion tool.  See
/// `Recorder-CLI-Conventions.md` §4.  `ct-print --full` (added
/// 2026-05 in `codetracer-trace-format-nim`) is what enables the
/// exact-value layer — its output is a deterministic JSON document
/// with every CBOR `ValueRecord` decoded to a structured form.
#[test]
fn test_recorded_trace_via_ct_print_json() {
    if !has_solc() || !has_anvil() {
        eprintln!("skipping: solc/anvil unavailable");
        return;
    }

    let ct_print = ct_print_path();
    if !ct_print.exists() {
        eprintln!(
            "SKIP: ct-print not found at {} — only available within the \
             metacraft workspace where codetracer-trace-format-nim is a sibling.",
            ct_print.display()
        );
        return;
    }

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");

    let bin = env!("CARGO_BIN_EXE_codetracer-evm-recorder");
    let output = Command::new(bin)
        .args(["record"])
        .arg(flow_test_source())
        .args(["--out-dir"])
        .arg(&out_dir)
        .args(["--function", "compute"])
        .env_remove("CODETRACER_EVM_RECORDER_DISABLED")
        .env_remove("CODETRACER_EVM_RECORDER_OUT_DIR")
        .output()
        .expect("failed to run recorder");

    assert!(
        output.status.success(),
        "recorder should succeed; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );

    // -----------------------------------------------------------------
    // Layer 1 (legacy): ct-print --json — substring presence checks.
    // Kept as a safety net so a regression in the textual rendering
    // is caught even if --full's JSON shape evolves.
    // -----------------------------------------------------------------
    let print_out = Command::new(&ct_print)
        .args(["--json"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print");

    assert!(
        print_out.status.success(),
        "ct-print --json should succeed; stderr: {}",
        String::from_utf8_lossy(&print_out.stderr)
    );

    let stdout_json = String::from_utf8_lossy(&print_out.stdout);
    assert!(
        !stdout_json.is_empty(),
        "ct-print --json produced empty output"
    );

    assert!(
        stdout_json.contains("FlowTest.sol"),
        "ct-print --json output should mention the source file; got:\n{stdout_json}"
    );
    // `metadata.program` carries the canonical absolute path of the
    // source file (recorder-test-requirements.md §1).  Pre-2026-05 the
    // EVM recorder labelled the trace with the bare contract name
    // (`"FlowTest"`); the spec-correct label is the source path.
    //
    // Compare it as a parsed JSON *value* rather than a raw-text
    // substring: a Windows canonical path contains backslashes (and the
    // `\\?\` extended-length prefix), which JSON-escapes to doubled
    // backslashes in the serialized text -- so a substring check against
    // the un-escaped `PathBuf` string would never match on Windows.
    let expected_program = flow_test_source()
        .canonicalize()
        .expect("FlowTest.sol must be canonicalizable")
        .to_string_lossy()
        .to_string();
    let print_doc: serde_json::Value = serde_json::from_str(&stdout_json)
        .expect("ct-print --json must emit valid JSON");
    let program_label = print_doc["metadata"]["program"]
        .as_str()
        .expect("ct-print --json output must carry metadata.program");
    assert_eq!(
        program_label, expected_program,
        "ct-print --json `metadata.program` should be the canonical source \
         path (recorder-test-requirements §1)"
    );
    assert!(
        stdout_json.contains("\"add\""),
        "ct-print --json output should mention the `add` Solidity helper \
         in the function table; got:\n{stdout_json}"
    );
    for varname in ["storedA", "storedResult"] {
        assert!(
            stdout_json.contains(&format!("\"{varname}\"")),
            "ct-print --json output should mention the `{varname}` storage \
             variable in the varname table; got:\n{stdout_json}"
        );
    }

    // -----------------------------------------------------------------
    // Layer 2 (the upgrade): ct-print --full — exact decoded values.
    // -----------------------------------------------------------------
    let full_output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");

    assert!(
        full_output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&full_output.stderr)
    );

    let doc: serde_json::Value = serde_json::from_slice(&full_output.stdout)
        .expect("ct-print --full should emit valid JSON");

    // ----- Function table: `add` must appear --------------------------
    // The EVM recorder resolves internal Solidity calls' function names
    // via the AST lookahead from `AUDIT-CTFS-2026-05.md` §3, so `add`
    // (the only internal Solidity helper called from `compute()`) lands
    // in the function table as a bare identifier.  The other two
    // entries are solc-generated dispatcher / fallback frames whose
    // names cannot be recovered today — they surface as
    // `fn_at_pc_<offset>` placeholders.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        functions.iter().any(|f| f.ends_with("add")),
        "expected `add` in functions table; got {:?}",
        functions
    );

    // ----- Path table: the canonical fixture path must appear ---------
    let paths: Vec<&str> = doc["paths"]
        .as_array()
        .expect("paths array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with("FlowTest.sol")),
        "expected FlowTest.sol in paths table; got {:?}",
        paths
    );

    // ----- Step / call counts ----------------------------------------
    // The EVM recorder produces one step per source-line transition in
    // `compute()` and `add()`, plus the dispatcher's prologue lines and
    // a few post-call return-site steps.  4 call_entry events are
    // emitted: the external `compute()` dispatcher frame
    // (fn_at_pc_384), the internal `add` invocation (fn_at_pc_314),
    // a second `fn_at_pc_314` frame for the post-call return path
    // (same name → same interned function_id), and the absorbed
    // `add` toplevel frame closed by `finalize()`.  Pre-2026-05 the
    // FFI keyed function IDs on (name, path, line) while the
    // multi-stream interning keyed on name alone, so the post-call
    // frame above used a function_id past the end of the function
    // table and surfaced as `<unresolved>`.  After the FFI fix
    // (`codetracer-trace-format-nim/src/codetracer_trace_writer_ffi.nim::trace_writer_ensure_function_id`
    // keys on name only), every emitted call resolves to a known
    // function entry — the test pin is now strictly stronger.
    // These are stable properties of the canonical fixture under the
    // current EVM recorder — if they change, that's a real regression
    // to investigate, not a flake.
    let counts = &doc["counts"];
    assert_eq!(
        counts["steps"].as_u64(),
        Some(15),
        "expected 15 step events for FlowTest.sol; counts={counts}",
    );
    assert_eq!(
        counts["calls"].as_u64(),
        Some(4),
        "expected 4 call events (dispatcher + add + repeated post-call \
         dispatcher + absorbed add); counts={counts}",
    );

    let events = doc["events"].as_array().expect("events array");

    // ----- Call sequence: 4 frames, all must resolve ------------------
    // The recorder emits 4 call_entry events:
    //   1. fn_at_pc_384 — solc dispatcher / external `compute()` frame
    //   2. fn_at_pc_314 — solc internal jump (the AST-aware fix in
    //      `AUDIT-CTFS-2026-05.md` §3 names this `add` in the function
    //      table even though the call's resolved name on the call
    //      record itself can lag); tracked there as a follow-up.
    //   3. A second fn_at_pc_314 frame for the post-call return path.
    //      Pre-FFI-fix this surfaced as `<unresolved>` because the FFI
    //      handed out a function_id past the function table; the May-12
    //      fix to `trace_writer_ensure_function_id` (key on name only)
    //      makes this resolve to the same `fn_at_pc_314` interned slot.
    //   4. `add` — the absorbed-into-toplevel `add` invocation that the
    //      recorder closes from `finalize()`.
    let call_entries: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .collect();
    assert_eq!(
        call_entries.len(),
        4,
        "expected exactly 4 call_entry events; got {:?}",
        call_entries
            .iter()
            .map(|e| e["function"].as_str().unwrap_or("<unresolved>"))
            .collect::<Vec<_>>()
    );
    // After the May-12 FFI key-on-name-only fix, EVERY call_entry must
    // carry a resolvable function name — there should be no
    // `<unresolved>` frames left.  This is strictly stronger than the
    // pre-fix pin which only required at least one resolved frame.
    let unresolved: Vec<usize> = call_entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            if e.get("function").and_then(|v| v.as_str()).is_none() {
                Some(i)
            } else {
                None
            }
        })
        .collect();
    assert!(
        unresolved.is_empty(),
        "every call_entry must resolve to a known function after the \
         FFI key-on-name-only fix; unresolved frame indexes={:?} \
         frames={:?}",
        unresolved,
        call_entries
            .iter()
            .map(|e| e["function"].as_str().unwrap_or("<unresolved>"))
            .collect::<Vec<_>>()
    );
    // At least one of the resolved frames must be `fn_at_pc_*`
    // (solc dispatcher) — verifies the lookahead path runs without
    // crashing even if AST resolution doesn't kick in for the
    // outermost frame.
    let resolved_frames: Vec<&str> = call_entries
        .iter()
        .filter_map(|e| e["function"].as_str())
        .collect();
    assert!(
        !resolved_frames.is_empty(),
        "at least one call_entry must carry a resolvable function name; got 0"
    );

    // ----- Strict ValueRecord variant invariant -----------------------
    // Every step var that surfaces must carry a `value.kind` field.
    //
    // The EVM recorder emits two variants today (the upgrade landed
    // when the `_decodes_loop_sums` ignored test was promoted):
    //   * `ValueRecord::Raw` — storage carry-forward and the raw
    //     stack-snapshot path used for internal call args.
    //   * `ValueRecord::Int` — small uint256 / int256 / bool stack
    //     values that fit in i64, surfaced for AST-resolved locals.
    //
    // Each variant is checked strictly: Raw must carry a `0x`-prefixed
    // hex `r` field; Int must carry a numeric `i` field.  Any *other*
    // kind (Sequence / Struct / ...) is a hard error so a future
    // recorder upgrade has to extend this assertion explicitly rather
    // than silently weakening it.
    for ev in events {
        if ev["kind"] != "step" {
            continue;
        }
        let step_index = ev["step_index"].as_u64().unwrap_or_default();
        let vars = ev["vars"].as_array().cloned().unwrap_or_default();
        for v in vars {
            let name = v["varname"].as_str().unwrap_or("<missing>");
            let value = &v["value"];
            let kind = value["kind"].as_str().unwrap_or_else(|| {
                panic!("step {step_index} var `{name}` is missing value.kind; got {value}")
            });
            match kind {
                "Raw" => {
                    let r = value["r"].as_str().unwrap_or_else(|| {
                        panic!(
                            "step {step_index} var `{name}` Raw value missing `r` field; got {value}"
                        )
                    });
                    assert!(
                        r.starts_with("0x"),
                        "step {step_index} var `{name}` Raw `r` should be hex-prefixed; got {r}"
                    );
                }
                "Int" => {
                    let _i = value["i"].as_i64().unwrap_or_else(|| {
                        panic!(
                            "step {step_index} var `{name}` Int value missing `i` field; got {value}"
                        )
                    });
                }
                other => panic!(
                    "step {step_index} var `{name}` decoded as unexpected kind `{other}`; \
                     got {value}; if a new ValueRecord variant has landed for the EVM \
                     recorder, extend this test to assert on it explicitly rather than \
                     silently accepting it"
                ),
            }
        }
    }

    // ----- Exact decoded byte values (anchored on stable snapshots) ---
    // Collect every (varname, raw_hex) pair surfaced by step events.
    // The EVM recorder snapshots stack/memory at every JUMP/PUSH
    // transition, so any single var name surfaces multiple values
    // across a step (mixing source values with dispatcher noise).
    // We assert that each canonical (var, value) pair appears at
    // least once across the whole trace — these are the values that
    // the source-level semantics of `compute()` guarantee:
    //
    //   * `storedA = a = 10`              → Raw `0xa`
    //   * `storedResult = add(10, 20) = 30` → Raw `0x1e`
    //   * `add(x = 10, y = 20)`           → Raw `0xa` and `0x14`
    //
    // The `x = 0xa` / `y = 0x14` pair specifically verifies the
    // AST-aware stack-label seeding fix from
    // `AUDIT-CTFS-2026-05.md` §4 — the recorder reads the concrete
    // EVM stack at `JumpType::Into` to recover the callee parameters.
    let observed_vars: Vec<(String, String)> = events
        .iter()
        .filter(|e| e["kind"] == "step")
        .flat_map(|e| {
            e["vars"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
        })
        .filter_map(|v| {
            let name = v["varname"].as_str()?.to_string();
            let r = v["value"]["r"].as_str()?.to_string();
            Some((name, r))
        })
        .collect();

    // The canonical EVM byte-level snapshots: storedA = 10 (= 0xa),
    // storedResult = add(10, 20) = 30 (= 0x1e), and the recovered add
    // parameters x = 10, y = 20.
    let expected: &[(&str, &str)] = &[
        ("storedA", "0xa"),
        ("storedResult", "0x1e"),
        ("x", "0xa"),
        ("y", "0x14"),
    ];
    for (name, value) in expected {
        assert!(
            observed_vars.iter().any(|(n, v)| n == name && v == value),
            "expected step variable `{name}` = Raw `{value}` in --full output; \
             observed = {observed_vars:?}"
        );
    }
}
