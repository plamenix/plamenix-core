# plamenix-core command runner.

# Show available recipes when invoked with no arguments.
default:
    @just --list

# Build every crate in the workspace.
build:
    cargo build --workspace

# Build every crate for release.
build-release:
    cargo build --workspace --release

# Run every crate's test suite, including doctests and the vendored
# driver. The vendor crate is consumed through [patch.crates-io] rather
# than as a workspace member, so --workspace does not reach its tests.
test: test-vendor
    cargo test --workspace
    cargo test --workspace --doc

# Run the vendored rsfbclient-native patch's own unit tests.
test-vendor:
    cd vendor/rsfbclient-native && cargo test --lib

# Run rustfmt across the whole workspace.
fmt:
    cargo fmt --all

# Verify rustfmt formatting without modifying files. Used in CI.
fmt-check:
    cargo fmt --all -- --check

# Run clippy with all targets and treat warnings as errors.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Build and open the API documentation.
doc:
    cargo doc --workspace --no-deps --open

# Build the API documentation without opening a browser.
doc-build:
    cargo doc --workspace --no-deps

# Run the full local CI pipeline (matches what CI runs).
all: fmt-check lint test doc-build

# Regenerate the TypeScript wire-type bindings consumed by plamenix-ui.
# Writes to ../plamenix-ui/src/db/generated.ts (override with an arg).
ts-gen *path:
    cargo run -p plamenix-ts-gen -- {{path}}

# Remove build artifacts.
clean:
    cargo clean
