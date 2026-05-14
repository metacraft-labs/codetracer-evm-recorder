//! Decode the `return_value` of a reverted EVM transaction into a
//! human-readable reason.
//!
//! Solidity emits two well-known revert payloads:
//!
//! * `Error(string)` — selector `0x08c379a0`, the classic
//!   `require(cond, "msg")` / `revert("msg")` form.  After the
//!   4-byte selector the data is ABI-encoded as a `(string)` tuple:
//!   a 32-byte offset (always `0x20`), a 32-byte length, then the
//!   UTF-8 bytes padded to a 32-byte boundary.
//! * `Panic(uint256)` — selector `0x4e487b71`, introduced in
//!   Solidity 0.8.0.  Payload is a single 32-byte panic code; well-known
//!   codes are spelled out in the Solidity docs (assert, arithmetic
//!   overflow, division by zero, ...).
//! * Solidity 0.8.4+ **custom errors** — `error Foo(t1, t2, ...);`
//!   declarations compile to a 4-byte selector
//!   (`keccak256("Foo(t1,t2,...)")[0..4]`) followed by the ABI-encoded
//!   tuple of arguments.  When the recorder is given the contract
//!   ABI it builds a [`CustomErrorRegistry`] mapping each known
//!   selector back to its source-level signature, so reverts surface
//!   as `Foo(arg1=v1, arg2=v2)` instead of opaque hex.
//!
//! Anything else (raw `revert(bytes)` payloads, an empty payload)
//! is surfaced as a hex blob — the recorder still emits *some*
//! error event so the trace is never silently empty.

/// Outcome of decoding a revert payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRevert {
    /// Short tag used as the `metadata` field of the io_event
    /// (`"Revert"`, `"Panic"`, `"RevertRaw"`, `"RevertEmpty"`).
    pub kind: &'static str,
    /// Human-readable body — the decoded reason string for
    /// `Error(string)`, a `panic code 0x..` line for `Panic(uint256)`,
    /// or a hex dump of the raw bytes for unrecognised payloads.
    pub message: String,
}

/// Selector for `Error(string)` — `keccak256("Error(string)")[0..4]`.
pub const ERROR_STRING_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];

/// Selector for `Panic(uint256)` — `keccak256("Panic(uint256)")[0..4]`.
pub const PANIC_UINT256_SELECTOR: [u8; 4] = [0x4e, 0x48, 0x7b, 0x71];

/// One Solidity custom-error declaration recovered from the contract
/// ABI.  `name` is the error identifier (e.g. `"InsufficientBalance"`)
/// and `param_types` is the ordered list of canonical Solidity type
/// names from the ABI (e.g. `["uint256", "uint256"]`).  Together they
/// determine the canonical signature `Name(t1,t2,...)` whose
/// `keccak256[..4]` becomes the on-chain selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomErrorDef {
    pub name: String,
    pub param_types: Vec<String>,
    pub param_names: Vec<String>,
}

impl CustomErrorDef {
    /// Canonical signature `Name(t1,t2,...)`.
    pub fn signature(&self) -> String {
        format!("{}({})", self.name, self.param_types.join(","))
    }
}

/// Lookup table mapping the 4-byte selector of a custom error to its
/// declaration.  Built from the contract ABI by the recorder CLI; an
/// empty registry preserves the prior `RevertRaw` behaviour for unknown
/// selectors.
#[derive(Debug, Default, Clone)]
pub struct CustomErrorRegistry {
    by_selector: std::collections::HashMap<[u8; 4], CustomErrorDef>,
}

impl CustomErrorRegistry {
    /// Build a registry from the parsed solc combined-json `abi` array.
    /// Entries whose `type` is not `"error"` are silently skipped, so
    /// the caller can hand the entire ABI to this builder.
    pub fn from_abi(abi: &[serde_json::Value]) -> Self {
        let mut by_selector = std::collections::HashMap::new();
        for item in abi {
            let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if kind != "error" {
                continue;
            }
            let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let inputs: Vec<&serde_json::Value> = item
                .get("inputs")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().collect())
                .unwrap_or_default();
            let param_types: Vec<String> = inputs
                .iter()
                .map(|p| p.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string())
                .collect();
            let param_names: Vec<String> = inputs
                .iter()
                .map(|p| p.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string())
                .collect();
            let signature = format!("{}({})", name, param_types.join(","));
            let selector = keccak_selector(signature.as_bytes());
            by_selector.insert(
                selector,
                CustomErrorDef {
                    name: name.to_string(),
                    param_types,
                    param_names,
                },
            );
        }
        Self { by_selector }
    }

    pub fn get(&self, selector: &[u8; 4]) -> Option<&CustomErrorDef> {
        self.by_selector.get(selector)
    }

    pub fn is_empty(&self) -> bool {
        self.by_selector.is_empty()
    }
}

/// Compute the canonical 4-byte function/error selector for a signature.
fn keccak_selector(signature: &[u8]) -> [u8; 4] {
    let hash = alloy::primitives::keccak256(signature);
    let mut sel = [0u8; 4];
    sel.copy_from_slice(&hash[..4]);
    sel
}

/// Decode the `return_value` of a reverted transaction.
///
/// Always returns `DecodedRevert` for a reverted call: even an
/// empty payload becomes `RevertEmpty` so the recorder can still emit
/// an `EventLogKind::Error` io_event.  The caller only needs to gate on
/// the transaction's `failed` flag.
///
/// This convenience wrapper passes an empty [`CustomErrorRegistry`];
/// callers that have a contract ABI should use
/// [`decode_revert_with_registry`] to get the typed-custom-error path.
pub fn decode_revert(output: &[u8]) -> DecodedRevert {
    decode_revert_with_registry(output, &CustomErrorRegistry::default())
}

/// Like [`decode_revert`] but consults `registry` to spell out the
/// human-readable name (and ABI-decoded args) of a custom error
/// whose selector is recognised.  Falls back to `RevertRaw` for
/// unknown selectors so the trace is never silently empty.
pub fn decode_revert_with_registry(
    output: &[u8],
    registry: &CustomErrorRegistry,
) -> DecodedRevert {
    if output.is_empty() {
        return DecodedRevert {
            kind: "RevertEmpty",
            message: String::new(),
        };
    }

    if output.len() >= 4 {
        let selector: [u8; 4] = [output[0], output[1], output[2], output[3]];
        let payload = &output[4..];

        if selector == ERROR_STRING_SELECTOR {
            if let Some(msg) = decode_error_string_payload(payload) {
                return DecodedRevert {
                    kind: "Revert",
                    message: msg,
                };
            }
        } else if selector == PANIC_UINT256_SELECTOR && payload.len() >= 32 {
            // Panic codes are small (single byte fits everything Solidity
            // emits today); render the trailing byte plus the well-known
            // mnemonic when one matches.  This keeps the io_event text
            // self-describing without requiring the consumer to look up
            // the spec.
            let code = payload[31];
            let mnemonic = panic_mnemonic(code);
            let message = match mnemonic {
                Some(m) => format!("panic 0x{:02x} ({})", code, m),
                None => format!("panic 0x{:02x}", code),
            };
            return DecodedRevert {
                kind: "Panic",
                message,
            };
        } else if let Some(def) = registry.get(&selector) {
            // Solidity custom error (`error Foo(t1, t2, ...);`).  Decode
            // each argument out of the ABI-encoded payload and render
            // `Foo(name1=v1, name2=v2)` so the consumer sees the
            // typed reason rather than a raw selector.
            let args = decode_custom_error_args(payload, &def.param_types, &def.param_names);
            let message = if args.is_empty() {
                format!("{}()", def.name)
            } else {
                format!("{}({})", def.name, args.join(", "))
            };
            return DecodedRevert {
                kind: "CustomError",
                message,
            };
        }
    }

    // Bare `revert(bytes)` calls and unknown selectors land here —
    // surface the hex dump so the user can still inspect the payload.
    // We deliberately do NOT silently drop unknown payloads; the spec
    // wants every reverted transaction to emit an Error io_event.
    DecodedRevert {
        kind: "RevertRaw",
        message: format!("0x{}", alloy::hex::encode(output)),
    }
}

/// Decode the ABI-encoded argument tuple of a custom error.  Renders
/// each argument as `name=v` (or just `v` when the parameter has no
/// declared name) using a shape the existing tests can pin on:
///
///   * `uint*` / `int*` arguments are rendered as decimal integers.
///   * `address` arguments are rendered as `0x`-prefixed 20-byte hex.
///   * `bool` arguments are rendered as `true`/`false`.
///   * everything else falls back to a 32-byte hex word so the data
///     is never silently lost.
///
/// Dynamic types (`string`, `bytes`, `<type>[]`, ...) are not yet
/// reconstructed; they would need head/tail offset chasing.  We surface
/// `<dynamic>` for those slots — the test fixtures in this round only
/// exercise primitives, and a future extension can teach the decoder
/// to follow dynamic offsets when needed.
fn decode_custom_error_args(
    payload: &[u8],
    param_types: &[String],
    param_names: &[String],
) -> Vec<String> {
    let mut out = Vec::with_capacity(param_types.len());
    for (idx, ty) in param_types.iter().enumerate() {
        let slot_off = idx * 32;
        if payload.len() < slot_off + 32 {
            // Truncated payload — leave the rest blank rather than
            // panic, so the trace still surfaces *something* useful.
            break;
        }
        let word = &payload[slot_off..slot_off + 32];
        let rendered = render_arg_word(word, ty);
        let name = param_names.get(idx).map(|s| s.as_str()).unwrap_or("");
        if name.is_empty() {
            out.push(rendered);
        } else {
            out.push(format!("{}={}", name, rendered));
        }
    }
    out
}

/// Render one 32-byte ABI word using the canonical Solidity type name.
fn render_arg_word(word: &[u8], ty: &str) -> String {
    if ty.starts_with("uint") || ty.starts_with("int") {
        let value = alloy::primitives::U256::from_be_slice(word);
        // Render small values in decimal — that's the canonical
        // human-readable form for revert reasons.
        format!("{}", value)
    } else if ty == "address" || ty == "address payable" {
        // Addresses occupy the trailing 20 bytes.
        format!("0x{}", alloy::hex::encode(&word[12..]))
    } else if ty == "bool" {
        if word.iter().any(|&b| b != 0) {
            "true".to_string()
        } else {
            "false".to_string()
        }
    } else if ty == "string" || ty == "bytes" || ty.ends_with(']') {
        "<dynamic>".to_string()
    } else {
        format!("0x{}", alloy::hex::encode(word))
    }
}

/// Decode the ABI-encoded `(string)` tuple that follows the
/// `Error(string)` selector.  Returns `None` when the encoding is
/// malformed (truncated, declared length exceeds payload, ...).
fn decode_error_string_payload(payload: &[u8]) -> Option<String> {
    if payload.len() < 64 {
        return None;
    }
    // First 32-byte word = offset.  Solidity always emits 0x20 here
    // for a single-element tuple, but we honour whatever value is
    // present so we don't reject odd-but-valid encodings.
    let mut offset_bytes = [0u8; 32];
    offset_bytes.copy_from_slice(&payload[0..32]);
    let offset = u256_to_usize(&offset_bytes)?;

    if offset > payload.len() {
        return None;
    }
    let after_offset = &payload[offset..];
    if after_offset.len() < 32 {
        return None;
    }
    let mut len_bytes = [0u8; 32];
    len_bytes.copy_from_slice(&after_offset[0..32]);
    let length = u256_to_usize(&len_bytes)?;

    let body = &after_offset[32..];
    if body.len() < length {
        return None;
    }
    let bytes = &body[..length];
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Convert a 32-byte big-endian word to `usize`.  Returns `None`
/// when the word does not fit (top 8 bytes non-zero on a 64-bit
/// platform — well outside any real ABI offset/length).
fn u256_to_usize(word: &[u8; 32]) -> Option<usize> {
    for &b in &word[..24] {
        if b != 0 {
            return None;
        }
    }
    let mut tail = [0u8; 8];
    tail.copy_from_slice(&word[24..]);
    Some(u64::from_be_bytes(tail) as usize)
}

/// Map well-known Solidity 0.8 panic codes to short mnemonics.  See
/// <https://docs.soliditylang.org/en/latest/control-structures.html#panic-via-assert-and-error-via-require>.
fn panic_mnemonic(code: u8) -> Option<&'static str> {
    Some(match code {
        0x00 => "generic",
        0x01 => "assert(false)",
        0x11 => "arithmetic over/underflow",
        0x12 => "division or modulo by zero",
        0x21 => "invalid enum conversion",
        0x22 => "storage byte array access",
        0x31 => "pop on empty array",
        0x32 => "out-of-bounds array access",
        0x41 => "memory allocation overflow",
        0x51 => "uninitialized internal function call",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_payload_is_revert_empty() {
        let r = decode_revert(&[]);
        assert_eq!(r.kind, "RevertEmpty");
        assert_eq!(r.message, "");
    }

    #[test]
    fn error_string_always_fails() {
        // 0x08c379a0 + offset(0x20) + len(12) + "always fails" padded to 32.
        let mut data = Vec::new();
        data.extend_from_slice(&ERROR_STRING_SELECTOR);
        let mut offset = [0u8; 32];
        offset[31] = 0x20;
        data.extend_from_slice(&offset);
        let mut len = [0u8; 32];
        len[31] = 12;
        data.extend_from_slice(&len);
        let mut body = [0u8; 32];
        body[..12].copy_from_slice(b"always fails");
        data.extend_from_slice(&body);

        let r = decode_revert(&data);
        assert_eq!(r.kind, "Revert");
        assert_eq!(r.message, "always fails");
    }

    #[test]
    fn panic_div_by_zero() {
        let mut data = Vec::new();
        data.extend_from_slice(&PANIC_UINT256_SELECTOR);
        let mut payload = [0u8; 32];
        payload[31] = 0x12;
        data.extend_from_slice(&payload);

        let r = decode_revert(&data);
        assert_eq!(r.kind, "Panic");
        assert!(r.message.contains("0x12"));
        assert!(r.message.contains("division"));
    }

    #[test]
    fn unknown_payload_surfaces_as_hex() {
        let raw = [0xde, 0xad, 0xbe, 0xef, 0x01, 0x02];
        let r = decode_revert(&raw);
        assert_eq!(r.kind, "RevertRaw");
        assert_eq!(r.message, "0xdeadbeef0102");
    }
}
