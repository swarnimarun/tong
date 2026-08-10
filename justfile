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

# Corpus: clone pinned entries, verify commits, populate homes/stores
# (the only networked phase). `--tier required|extended|all` selects rows.
corpus-fetch *args:
    cargo build -p tong
    cargo run -p tong-corpus-runner -- fetch {{ args }}

# Corpus: differential resolver equality vs `cargo metadata --offline`
corpus-resolve *args:
    cargo run -p tong-corpus-runner -- resolve {{ args }}

# Corpus: execute each entry's Tong gate offline
corpus-build *args:
    cargo run -p tong-corpus-runner -- build {{ args }}

# Corpus: print the accumulated report
corpus-report:
    cargo run -p tong-corpus-runner -- report
