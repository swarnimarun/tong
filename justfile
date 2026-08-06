# Thin recipes wrapping cargo. `just --list` shows all recipes.

# Install the pinned toolchain components (idempotent)
setup:
    rustup component add clippy rustfmt

# Build all workspace crates
build:
    cargo build --workspace

# Fast type-check without codegen
check:
    cargo check --workspace --all-targets

# Run all tests
test:
    cargo test --workspace

# Lint; warnings are denied
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Format all code (check with `just fmt --check`)
fmt *args:
    cargo fmt --all {{ args }}

# Run the tong CLI
run *args:
    cargo run -p tong -- {{ args }}
