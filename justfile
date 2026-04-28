# VidViewer justfile
# Common developer commands. Run `just` to see available recipes.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Format source
fmt:
    cargo fmt --all

# Check formatting (non-mutating)
fmt-check:
    cargo fmt --all -- --check

# Clippy, warnings as errors
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Type-check
check:
    cargo check --all-targets

# Run unit and integration tests
test:
    cargo test --all

# Run the server
run:
    cargo run

# Environment sanity check
doctor:
    cargo run -- doctor

# End-to-end smoke check
smoke:
    cargo run -- doctor
    cargo test --all

# Refresh sqlx offline metadata
prepare-sqlx:
    cargo sqlx prepare -- --all-targets

# Build an optimized release binary at target/release/vidviewer
build:
    cargo build --release --locked

# Install the release binary to ~/.cargo/bin (make sure that's on $PATH)
install:
    cargo install --path . --locked

# Line/region coverage report via cargo-llvm-cov. Prints a per-file
# summary to stdout and writes an HTML report to
# target/llvm-cov/html/index.html. Requires the `llvm-tools-preview`
# rustup component and `cargo-llvm-cov`; see docs/agents/debugging.md.
coverage:
    #!/usr/bin/env bash
    set -euo pipefail
    # `rustup component add llvm-tools-preview` installs llvm-cov and
    # llvm-profdata under the toolchain's lib dir, but cargo-llvm-cov
    # expects them on PATH. Point at them directly.
    tc="$(rustup show active-toolchain | awk '{print $1}')"
    bin="$HOME/.rustup/toolchains/$tc/lib/rustlib/$(rustc -vV | awk '/host:/ {print $2}')/bin"
    export LLVM_COV="$bin/llvm-cov"
    export LLVM_PROFDATA="$bin/llvm-profdata"
    cargo llvm-cov --all-targets --html
