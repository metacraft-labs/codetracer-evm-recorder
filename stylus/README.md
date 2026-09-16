# Stylus host-module configuration (M28)

This directory holds the Stylus-specific configuration consumed by
the **M27 generic WASM host-module instrumentation framework**
(`codetracer-wasm-host-module-framework`). It replaces the
hand-written Go file
`codetracer-wasm-recorder/internal/stylus/stylus_funcs.go` with a
data-driven path.

## Files

- `codetracer.toml` — the host-module description. Lists every
  `vm_hooks` import the Stylus runtime expects, its signature, and
  optional notes mapping each import to its EVM equivalent.

## How it is consumed

```
codetracer-wasm-host-module-framework::plan_from_toml(
    fs::read_to_string("codetracer-evm-recorder/stylus/codetracer.toml").unwrap()
) -> PassThroughPlan
```

The plan is a structured value the embedder translates to its
target language:

- **wazero** recorder (Go) — generates `wazero.HostModuleBuilder`
  stubs.
- **wasmtime** recorder (Rust) — generates closures.
- **browser** recorder (JS) — generates a `WebAssembly.Module`
  imports object.

`auto_correlation_markers = true` in `codetracer.toml` means every
crossing also emits an M25 correlation marker keyed by the WASM
arguments. This wires the recorder into the cross-process origin
algorithm (M29) without any Stylus-specific Go shim.

## Compatibility note

This file is the **single source of truth** for the Stylus import
list. The legacy Go code is retained in `codetracer-wasm-recorder`
on the `value-origin` branch until the parity tests have been
exercised end-to-end against a live Stylus toolchain. The M28 migration
plan lives with the CodeTracer specs, which are maintained internally.

The static surface is pinned in three places (M27 plan, hand-extracted
fixture, and — added 2026-06-18 — the live Go source parsed at test
time) by the parity tests in
`codetracer-wasm-instrumenter/crates/codetracer-wasm-host-module-framework/tests/stylus_parity.rs`.
What still needs a live host is the *runtime* three-way parity
(legacy Go path vs M27-on-wazero vs `ct instrument` rewriter); that
is driven by a one-shot runner script kept with the CodeTracer specs,
which are maintained internally.

Prereqs: `cargo-stylus`, a running `nitro-devnode` at `:8547`,
`go` >= 1.21, `rustup` `wasm32-unknown-unknown` target, `jq`.
Exit 0 from the runner is the gating signal for opening the PR
against `codetracer-wasm-recorder` that retires
`internal/stylus/{stylus.go,stylus_funcs.go,stylus_trace.go}` and
the `cmd/wazero --stylus` flag plumbing.
