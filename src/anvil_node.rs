//! Starting a local Anvil node for the `record` route and for the tests.
//!
//! Every Anvil this crate starts goes through [`spawn`], so the recorder
//! binary and the integration tests share one startup path and one error.
//!
//! [`alloy::node_bindings::Anvil::try_spawn`] reads Anvil's stdout until the
//! `Listening on <addr>` line and gives up with `NodeError::Timeout` once its
//! startup timeout (`NODE_STARTUP_TIMEOUT`, 10 s) has elapsed. The check runs
//! only before each line is read, so it catches an Anvil that has exited
//! (stdout at EOF) and one that is alive but slow; a silent Anvil blocks in
//! the read either way.
//!
//! The time counted against that timeout is not only Anvil's. For every
//! banner line after `Private Keys`, alloy derives a signer and an address
//! from each key read so far, about 350 secp256k1 scalar multiplications and
//! Keccak hashes per start. Unoptimised, that cost more than Anvil itself
//! (0.4 to 0.7 s against 0.05 to 0.27 s on a loaded Linux host), and it is
//! the likely reason a start on the much slower Windows CI guest exceeded
//! the timeout. `Cargo.toml` therefore optimises those crates in debug
//! builds, and the timeout stays alloy's default.

use std::time::Instant;

use alloy::node_bindings::{Anvil, AnvilInstance, NODE_STARTUP_TIMEOUT};
use eyre::WrapErr;

/// Start `builder`'s Anvil and wait for its listening address.
///
/// The returned [`AnvilInstance`] stops the process when it is dropped, so
/// the caller owns the node's lifetime. On failure the error says how long
/// the startup ran, which separates "Anvil could not start at all" from
/// "Anvil was still starting when the timeout ran out", and keeps alloy's
/// own error as its cause.
pub fn spawn(builder: Anvil) -> eyre::Result<AnvilInstance> {
    let started = Instant::now();
    builder.try_spawn().wrap_err_with(|| {
        format!(
            "anvil did not start: no `Listening on` line after {:.1} s (timeout {} s)",
            started.elapsed().as_secs_f64(),
            NODE_STARTUP_TIMEOUT.as_secs()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A startup that fails says so, names the timeout, and keeps alloy's
    /// own reason in the chain. A program path that does not exist is the
    /// one failure that needs no Anvil, so this test runs everywhere.
    #[test]
    fn a_failed_startup_reports_the_elapsed_time_and_the_cause() {
        let missing = std::env::temp_dir().join("codetracer-evm-recorder-no-such-anvil");
        let err = spawn(Anvil::at(&missing)).expect_err("a missing program cannot start");
        let message = format!("{err:#}");
        assert!(
            message.starts_with("anvil did not start: no `Listening on` line after "),
            "unexpected message: {message}"
        );
        assert!(
            message.contains("(timeout 10 s)"),
            "unexpected message: {message}"
        );
        assert!(
            message.contains("could not spawn node"),
            "alloy's spawn error should stay in the chain: {message}"
        );
    }
}
