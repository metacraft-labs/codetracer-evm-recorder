set shell := ["bash", "-euo", "pipefail", "-c"]

# Default recipe — list available recipes.
default:
  @just --list

build:
  cargo build

build-release:
  cargo build --release

test:
  cargo test
  bash tests/verify-cli-convention-no-silent-skip.sh

t: test

test-rust:
  cargo test

verify-cli-convention:
  bash tests/verify-cli-convention-no-silent-skip.sh

lint:
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  bash tests/verify-cli-convention-no-silent-skip.sh

format:
  cargo fmt

fmt: format
