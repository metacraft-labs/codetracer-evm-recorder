// Re-export alloy's Geth trace types for structLog processing.
//
// These types represent the output of `debug_traceTransaction` with the
// default (structLog) tracer.

pub use alloy::rpc::types::trace::geth::{DefaultFrame, StructLog};
