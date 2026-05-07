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
/// JSON via `ct-print --json` and assert on the textual representation.
///
/// `ct-print`'s JSON output owns its schema (owned by
/// `codetracer-trace-format-nim` and may evolve), and integer values
/// produced by the EVM recorder don't always round-trip cleanly through
/// `ct-print --json` today (same pre-existing limitation as Cardano
/// 1.48 / Circom 1.49).  We therefore assert on **structural anchors**
/// that the recorder must surface for any CodeTracer consumer:
///   * the source filename (`FlowTest.sol`),
///   * the program/contract name (`FlowTest`),
///   * the entry-point function name (`compute`).
///
/// This is the canonical workflow `Recorder-CLI-Conventions.md` §4
/// prescribes for content-level test assertions.
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

    // ct-print --json <file.ct>
    let print_out = Command::new(&ct_print)
        .args(["--json"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print");

    assert!(
        print_out.status.success(),
        "ct-print should succeed; stderr: {}",
        String::from_utf8_lossy(&print_out.stderr)
    );

    let stdout = String::from_utf8_lossy(&print_out.stdout);
    assert!(!stdout.is_empty(), "ct-print --json produced empty output");

    // Structural anchors that the recorder must surface for any
    // CodeTracer consumer to function:
    //   * the source filename (FlowTest.sol),
    //   * the program/contract name in the metadata (FlowTest),
    //   * at least one Solidity function name in the function table —
    //     `add` (the internal helper called from compute()) — verifying
    //     the AST-aware function-name resolution path lands in the .ct
    //     bundle.
    //   * the storage variable names (`storedA`, `storedResult`)
    //     in the varname table — verifying value-side staging.
    //
    // Note: the EVM recorder's integer values (10, 20, 30, ...)
    // currently don't round-trip through `ct-print --json` due to a
    // pre-existing limitation in the EVM recorder's Variable record
    // payload format (see AUDIT-CTFS-2026-05.md for the open
    // follow-up).  This is the same fall-back-to-structural-anchors
    // policy used by the Cardano (1.48) and Circom (1.49) audits.
    assert!(
        stdout.contains("FlowTest.sol"),
        "ct-print --json output should mention the source file; got:\n{stdout}"
    );
    assert!(
        stdout.contains("\"FlowTest\""),
        "ct-print --json output should mention the `FlowTest` contract / program; got:\n{stdout}"
    );
    assert!(
        stdout.contains("\"add\""),
        "ct-print --json output should mention the `add` Solidity helper \
         in the function table; got:\n{stdout}"
    );
    for varname in ["storedA", "storedResult"] {
        assert!(
            stdout.contains(&format!("\"{varname}\"")),
            "ct-print --json output should mention the `{varname}` storage \
             variable in the varname table; got:\n{stdout}"
        );
    }
}
