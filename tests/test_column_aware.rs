//! Column-aware step emission regression test.
//!
//! Mirrors the JS recorder's
//! `tests/integration/column-aware.test.ts` "multiple statements on
//! one line each record distinct columns" fixture, adapted for
//! Solidity: a single source line packs three statements
//! (`uint x = 1; uint y = 2; uint z = 3;`) so each one starts at a
//! distinct column.  The EVM recorder must:
//!
//!   * Set `meta.dat` bit 4 (`FLAG_HAS_COLUMN_AWARE_STEPS`).  Verified
//!     through `ct-print --full`'s `metadata.flags.has_column_aware_steps`.
//!   * Surface a step event for each of the three statements with a
//!     strictly distinct column value.  The columns are decoded by the
//!     reader from the writer-side global position table populated via
//!     `register_path_with_line_lengths`.
//!
//! See `codetracer-specs/Planned-Features/
//! Column-Aware-Navigation-Other-Languages.plan.md` for the acceptance
//! criteria (column-aware flag + distinct-columns assertion) and
//! `codetracer-trace-format-spec/trace-events.md` §"Column Encoding".

use std::path::{Path, PathBuf};
use std::process::Command;

fn ct_print_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("codetracer-trace-format-nim")
        .join(format!("ct-print{}", std::env::consts::EXE_SUFFIX))
}

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

/// Returns the path to ct-print or logs a `SKIP:` diagnostic and
/// returns `None`.  Mirrors the convention enforced by
/// `verify-cli-convention-no-silent-skip.sh`.
fn ct_print_or_skip(test_name: &str) -> Option<PathBuf> {
    if !has_solc() || !has_anvil() {
        eprintln!("SKIP: {test_name} requires solc + anvil on PATH (use the Nix dev shell).");
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

#[test]
fn test_column_aware_distinct_columns_on_one_line() {
    let Some(ct_print) = ct_print_or_skip("test_column_aware_distinct_columns_on_one_line") else {
        return;
    };

    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-programs")
        .join("column_aware")
        .join("ColumnAware.sol");
    assert!(source.exists(), "fixture missing: {}", source.display());

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let bin = env!("CARGO_BIN_EXE_codetracer-evm-recorder");
    let output = Command::new(bin)
        .arg("record")
        .arg(&source)
        .args(["--out-dir"])
        .arg(&out_dir)
        .args(["--function", "run"])
        .env_remove("CODETRACER_EVM_RECORDER_DISABLED")
        .env_remove("CODETRACER_EVM_RECORDER_OUT_DIR")
        .output()
        .expect("failed to run recorder");
    assert!(
        output.status.success(),
        "recorder CLI should succeed; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {out_dir:?}"
    );

    let dump = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");
    assert!(
        dump.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&dump.stderr),
    );

    let doc: serde_json::Value =
        serde_json::from_slice(&dump.stdout).expect("ct-print --full should emit valid JSON");

    // --- meta.dat bit 4: FLAG_HAS_COLUMN_AWARE_STEPS ---
    // The trace metadata must advertise column-aware support, which is
    // how downstream tooling decides whether to surface columns to the
    // user.  See the JS reference assertion at
    // `codetracer-js-recorder/tests/integration/column-aware.test.ts`.
    let has_column_aware = doc["metadata"]["flags"]["has_column_aware_steps"].as_bool();
    assert_eq!(
        has_column_aware,
        Some(true),
        "trace metadata must advertise has_column_aware_steps=true; got {:?}",
        doc["metadata"]
    );

    // --- gather step events on the three-statement line ---
    //
    // The fixture's body is (1-based line numbers):
    //
    //   18: function run() public pure returns (uint256) {
    //   19:     uint x = 1; uint y = 2; uint z = 3;
    //   20:     return x + y + z;
    //   21: }
    //
    // The three statements on line 19 start at columns 9, 21, and 33
    // (1-based byte offsets within the line — 8 spaces of indent + 1,
    // then +12 for each `uint X = N; ` chunk).
    //
    // The recorder lowers each step into a `(register_step,
    // write_delta_column)` pair on the writer wire — the first member
    // of each pair re-surfaces as a column-1 sekDeltaStep event in
    // ct-print's output (the writer resets the column cursor to col 1
    // of the target line on every `register_step`), so the *distinct*
    // user-visible landing columns on the multi-statement line are
    // the non-1 entries plus the single column-1 entry for the first
    // statement (`uint x = 1;`).
    //
    // Pre-fix (line-only navigation) the recorder collapsed all three
    // statements onto a single step at column 1 because the writer's
    // column cursor was never advanced.
    let events = doc["events"].as_array().expect("events array");
    let mut cols_by_line: std::collections::BTreeMap<i64, std::collections::BTreeSet<i64>> =
        std::collections::BTreeMap::new();
    for ev in events {
        if ev["kind"] != "step" {
            continue;
        }
        let Some(line) = ev["line"].as_i64() else {
            continue;
        };
        let Some(col) = ev["column"].as_i64() else {
            continue;
        };
        cols_by_line.entry(line).or_default().insert(col);
    }

    // Three statements on a single line MUST surface as three (or
    // more) distinct columns — the strict acceptance criterion from
    // the column-aware-navigation plan.  We pick the maximum-cardinality
    // line so the assertion is robust to the SPDX/comment block above
    // the contract drifting line numbers slightly; the fixture's
    // three-statement line is the *only* line in the program that
    // yields three or more distinct columns.
    let (line, distinct_cols) = cols_by_line
        .iter()
        .max_by_key(|(_, cols)| cols.len())
        .map(|(line, cols)| (*line, cols.clone()))
        .expect("trace should contain at least one step event with a column field");
    assert!(
        distinct_cols.len() >= 3,
        "expected the three-statement line to surface >= 3 distinct step columns; \
         got line {line} -> {distinct_cols:?}; full line->cols map: {cols_by_line:?}",
    );

    // Every surfaced column is >= 1 (1-based on the wire — sanity
    // check that the 0-based column from the solc source map was
    // correctly translated to the writer's 1-based encoding).
    for col in &distinct_cols {
        assert!(
            *col >= 1,
            "step column must be >= 1 (1-based on the wire); got {col} on line {line}",
        );
    }

    eprintln!(
        "PASS: column-aware step emission — line {line} surfaces distinct columns {distinct_cols:?}"
    );
}
