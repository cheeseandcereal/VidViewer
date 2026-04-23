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
