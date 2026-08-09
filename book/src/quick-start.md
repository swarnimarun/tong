# Quick Start

## Prerequisites

- A Rust toolchain (the repository pins stable `1.97.1` in
  `rust-toolchain.toml`; `rustup` picks it up automatically).
- `mdbook` only if you want to build the docs locally.
- Nothing else: Tong never invokes Cargo and needs no package manager at
  build time.

## Install

```sh
git clone git@github.com:swarnimarun/tong.git
cd tong
cargo build --release
```

The binary is `target/release/tong`; add it to your `PATH`:

```sh
export PATH="$PWD/target/release:$PATH"
```

## Hello, native mode

Native mode is a `Tong.toml` workspace — no `Cargo.toml` at all.

```sh
mkdir hello && cd hello
```

`Tong.toml`:

```toml
[workspace]
name = "hello"

[toolchain.rust]
kind = "system"

[target.hello]
rule = "rust_binary"
crate_root = "src/main.rs"
edition = "2021"
```

`src/main.rs`:

```rust
fn main() {
    println!("hello from tong");
}
```

Build and run:

```sh
tong build
# build complete: 1 actions (0 cached, 1 executed)
# artifact: .tong/out/dev/hello/hello

tong run :hello
# hello from tong
```

The binary lands at `.tong/out/dev/hello/hello` (`<profile>/<target>/`).
Run it directly if you prefer. Note that `[toolchain.rust] kind = "system"`
captures your local rustc and sysroot into the store and fingerprints
them — the build is hermetic *and* portable to the extent your toolchain
is.

## Hello, Cargo mode

Tong also builds plain Cargo workspaces with no `Tong.toml` — the mode is
chosen by whichever manifest exists. Use the shipped example:

```sh
cd examples/01-calc
tong build
tong run :calc_cli
tong test
```

`calc-cli` is a Cargo package name; `:calc_cli` finds it (dashes and
underscores are interchangeable in labels). Features, profiles, and
workspace inheritance all work as in Cargo — see
[Cargo Import Mode](cargo-import.html).

## What just happened

`tong build` ran the whole pipeline: manifest loading → resolution →
configured target graph → backend analysis → immutable action graph →
execution → content-addressed outputs. Details:

- Every action (here: one `rustc` invocation) has a digest covering its
  semantic fields, its input tree, and the toolchain closure. The second
  `tong build` executes zero actions (cache hit).
- Artifacts are materialized under `.tong/out/dev/`.
- The content-addressed store lives at `.tong/store/`; see
  [Caching and the Store](caching.html).

## Next steps

- [Usage Guide](usage.html) — every command, flag, and environment
  variable.
- [Manifest Reference](manifest-reference.html) — the full `Tong.toml`
  schema.
- [Examples](examples.html) — eight worked workspaces covering build
  scripts, proc macros, SDL3 interop, registry dependencies, and more.
