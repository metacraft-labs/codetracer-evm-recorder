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
//!
//! Anything else (custom errors, raw `revert(bytes)` payloads, an
//! empty payload) is surfaced as a hex blob — the recorder still
//! emits *some* error event so the trace is never silently empty.

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

/// Decode the `return_value` of a reverted transaction.
///
/// Always returns `Some(DecodedRevert)` for a reverted call: even an
/// empty payload becomes `RevertEmpty` so the recorder can still emit
/// an `EventLogKind::Error` io_event.  The caller only needs to gate on
/// the transaction's `failed` flag.
pub fn decode_revert(output: &[u8]) -> DecodedRevert {
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
        }
    }

    // Custom errors (selector + ABI args) and bare `revert(bytes)`
    // calls land here — surface the hex dump so the user can still
    // inspect the payload.  We deliberately do NOT silently drop
    // unknown payloads; the spec wants every reverted transaction to
    // emit an Error io_event.
    DecodedRevert {
        kind: "RevertRaw",
        message: format!("0x{}", alloy::hex::encode(output)),
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
