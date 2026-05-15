//! Pure Yul compile + source-map synthesis path.
//!
//! Solc's `--strict-assembly` mode is the canonical way to compile a
//! standalone Yul object (`.yul`) -- it doesn't accept the regular
//! `--combined-json abi,bin,...` invocation but instead emits the
//! deploy bytecode to stdout under a `Binary representation:` header
//! and an EVM assembly-JSON document under an `EVM assembly:` header.
//!
//! The assembly-JSON document carries one entry per asm instruction
//! with `begin`/`end` byte offsets into the source file.  The runtime
//! object's `.code` array (under `.data["0"][".code"]`) corresponds
//! 1:1 to the runtime bytecode's instruction sequence -- with one
//! caveat: `tag` entries are pure label declarations and emit zero
//! bytes, so we filter them out when synthesizing the source map.
//!
//! This module turns the asm-json output into the runtime
//! [`SourceMap`] consumed by the rest of the recorder pipeline.

use eyre::{Context, Result};

use crate::source_map::{JumpType, SourceMap, SourceMapEntry};

/// Output of the Yul compile path.
pub struct YulCompileOutput {
    /// Deploy bytecode (the constructor that copies the runtime to
    /// memory and RETURNs it).
    pub deploy_bytecode: Vec<u8>,
    /// Synthesized [`SourceMap`] for the runtime bytecode (one entry
    /// per emitted instruction, indexed by instruction position).
    pub runtime_source_map: SourceMap,
}

/// Run `solc --strict-assembly --bin --asm-json <yul_file>` and parse
/// the output into a [`YulCompileOutput`].
///
/// `solc_cmd` is the solc binary to invoke (mirrors the Solidity
/// path's `SOLC_PATH` env-var convention).
pub fn compile_yul(solc_cmd: &str, yul_file: &std::path::Path) -> Result<YulCompileOutput> {
    let output = std::process::Command::new(solc_cmd)
        .args(["--strict-assembly", "--bin", "--asm-json"])
        .arg(yul_file)
        .output()
        .with_context(|| format!("failed to run solc ({solc_cmd}) in --strict-assembly mode"))?;

    if !output.status.success() {
        return Err(eyre::eyre!(
            "solc --strict-assembly compilation failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8(output.stdout).context("solc output is not valid UTF-8")?;

    let deploy_bytecode = parse_binary_representation(&stdout)?;
    let asm_json_str = extract_asm_json(&stdout)?;
    let asm: serde_json::Value =
        serde_json::from_str(asm_json_str).context("solc --asm-json output is not valid JSON")?;

    // The runtime object lives at .data["0"][".code"].
    let runtime_code = asm
        .pointer("/.data/0/.code")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            eyre::eyre!(
                "solc --asm-json output missing the runtime `.data[\"0\"][\".code\"]` array"
            )
        })?;

    let runtime_source_map = synthesize_source_map(runtime_code)?;

    Ok(YulCompileOutput {
        deploy_bytecode,
        runtime_source_map,
    })
}

/// Extract the hex-encoded deploy bytecode from solc's
/// `Binary representation:` header section.
fn parse_binary_representation(stdout: &str) -> Result<Vec<u8>> {
    // Solc emits the bytecode as a single hex line under
    // `Binary representation:`.  We grab the first non-empty,
    // hex-only line after the marker.
    let marker = "Binary representation:";
    let after = stdout
        .find(marker)
        .ok_or_else(|| eyre::eyre!("solc output missing `{marker}` header"))?
        + marker.len();
    let hex_line = stdout[after..]
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| eyre::eyre!("solc output: no hex line after `{marker}`"))?;
    alloy::hex::decode(hex_line).context("solc deploy bytecode is not valid hex")
}

/// Extract the JSON document under solc's `EVM assembly:` header.
fn extract_asm_json(stdout: &str) -> Result<&str> {
    let marker = "EVM assembly:";
    let start = stdout
        .find(marker)
        .ok_or_else(|| eyre::eyre!("solc output missing `{marker}` header"))?
        + marker.len();
    // The JSON document occupies one line and starts with `{`.
    let after = &stdout[start..];
    let json_start = after
        .find('{')
        .ok_or_else(|| eyre::eyre!("solc output: no `{{` after `{marker}`"))?;
    let after_brace = &after[json_start..];
    // The JSON spans to the end of the line (solc emits it on a
    // single line).
    let line_end = after_brace.find('\n').unwrap_or(after_brace.len());
    Ok(after_brace[..line_end].trim())
}

/// Walk the asm-json runtime instruction list and synthesize one
/// [`SourceMapEntry`] per emitted bytecode instruction.
///
/// `tag` entries declare labels and emit zero bytes -- they're
/// filtered out so the resulting source map is indexed exactly as the
/// bytecode's `pc -> instruction index` map.
///
/// `jumpType` field on the asm-json entries (`[in]`, `[out]`) maps to
/// [`JumpType::Into`] / [`JumpType::OutOf`] so the recorder's
/// internal-call-detection logic surfaces Yul function calls as
/// nested call frames (the headline strict-pin requirement for the
/// `yul_pure_test` fixture).
fn synthesize_source_map(asm_code: &[serde_json::Value]) -> Result<SourceMap> {
    let mut entries = Vec::new();
    for inst in asm_code {
        let name = inst
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("asm-json instruction missing `name`"))?;
        if name == "tag" {
            // Pure label -- emits no bytes, skip.
            continue;
        }
        let begin = inst.get("begin").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let end = inst.get("end").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let length = (end - begin).max(0);
        let jump_type = match inst.get("jumpType").and_then(|v| v.as_str()) {
            Some("[in]") => JumpType::Into,
            Some("[out]") => JumpType::OutOf,
            _ => JumpType::Regular,
        };
        entries.push(SourceMapEntry {
            offset: begin,
            length,
            // Yul has a single source file -- always file_index 0.
            file_index: 0,
            jump_type,
            modifier_depth: 0,
        });
    }
    Ok(SourceMap::from_entries(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesize_source_map_filters_tags_and_maps_jumps() {
        let asm = serde_json::json!([
            {"begin": 10, "end": 20, "name": "PUSH", "value": "1"},
            {"begin": 30, "end": 40, "name": "tag", "value": "1"},
            {"begin": 30, "end": 40, "name": "JUMPDEST"},
            {"begin": 50, "end": 60, "name": "JUMP", "jumpType": "[in]"},
            {"begin": 70, "end": 80, "name": "JUMP", "jumpType": "[out]"},
            {"begin": 90, "end": 100, "name": "ADD"},
        ]);
        let asm_arr = asm.as_array().unwrap();
        let map = synthesize_source_map(asm_arr).unwrap();
        assert_eq!(map.len(), 5, "tag entry must be filtered out");
        assert_eq!(map.get(0).unwrap().offset, 10);
        assert_eq!(map.get(0).unwrap().length, 10);
        assert_eq!(map.get(1).unwrap().offset, 30);
        assert_eq!(map.get(2).unwrap().jump_type, JumpType::Into);
        assert_eq!(map.get(3).unwrap().jump_type, JumpType::OutOf);
        assert_eq!(map.get(4).unwrap().jump_type, JumpType::Regular);
    }
}
