# Tong

> STATUS: VIBE CODED -- testing some ideas with 

> ROADMAP: Initial idea creation with AI MODELS => then a guided plan to build practical implementation => get to the final implementation only supporting Rust and C for now.

Tong is an experimental Rust-first hermetic build system. The current implementation is an MVP that can build simple Rust libraries and binaries from `Cargo.toml` or `Tong.toml` without depending on Cargo's build execution model.

## Current Status

Implemented:

- `tong build`
- `tong fetch`
- `tong plan`
- `tong add`
- `tong clean`
- Manifest discovery for `Tong.toml` and `Cargo.toml`
- Basic `[package]`, `[lib]`, `[[bin]]`, and path `[dependencies]`
- Git, tar, `.crate`, and zip dependency source materialization
- `tong.sources.*` overrides for registry-style transitive dependencies
- Basic `build.rs` compile/run support
- Basic proc-macro crate support
- Direct `rustc` compilation for Rust libraries and binaries
- Per-action cache keys
- Clean action environments via `env_clear`
- Explicit action inputs and outputs
- A language backend trait intended for future Zig, Nim, Swift, JVM, and other rules

Not implemented yet:

- crates.io resolution
- native dependency fetching
- rpath/install-name/DLL fixups
- OS-level sandboxing
- remote cache

## Quick Start

Build Tong itself:

```sh
cargo build -p tong
```

Build the sample project:

```sh
cargo run -p tong -- build examples/simple-rust-project
```

Run the produced binary:

```sh
./examples/simple-rust-project/target/tong/debug/bin/hello-tong
```

Inspect the discovered package graph:

```sh
cargo run -p tong -- plan examples/simple-rust-project
```

Build a project that uses `Tong.toml`:

```sh
cargo run -p tong -- build examples/tong-manifest-project
```

Build a project with a local path dependency:

```sh
cargo run -p tong -- build examples/path-dep-project
```

Build a project with a hermetic `build.rs` action:

```sh
cargo run -p tong -- build examples/build-script-project
```

Build a small clap-based CLI from fixed source URLs. This example uses tar/`.crate` sources for the clap stack, plus tiny local git and zip source dependencies to exercise all source kinds:

```sh
cargo run -p tong -- build examples/cli-mini
./examples/cli-mini/target/tong/debug/bin/cli-mini echo hello
./examples/cli-mini/target/tong/debug/bin/cli-mini cat README.md
```

Prefetch and resolve dependency sources without compiling:

```sh
cargo run -p tong -- fetch examples/cli-mini
```

Add a source dependency:

```sh
cargo run -p tong -- add clap --tar https://static.crates.io/crates/clap/clap-4.5.54.crate --sha256 <sha256> --features derive,std --no-default-features --manifest-path examples/cli-mini/Tong.toml
```

Supported source forms:

```toml
[dependencies]
from_git = { git = "https://example.com/repo.git", rev = "abc123" }
from_tar = { tar = "https://example.com/pkg.tar.gz", sha256 = "..." }
from_zip = { zip = "https://example.com/pkg.zip", sha256 = "..." }
```

Registry-style transitive dependencies can be resolved explicitly with source overrides:

```toml
[tong.sources.clap]
tar = "https://static.crates.io/crates/clap/clap-4.5.54.crate"
sha256 = "..."
```

## Project Steps

1. Bootstrap the Rust workspace and CLI.
2. Load `Cargo.toml` and `Tong.toml` manifests.
3. Build a package graph from local path dependencies.
4. Lower Rust packages into explicit compile actions.
5. Run actions with a clean environment.
6. Compute cache keys from tools, args, env, inputs, outputs, profile, and platform.
7. Store artifacts under `target/tong`.
8. Add `build.rs` as a hermetic action.
9. Add fixed-output source downloads and a content-addressed source store.
10. Add native dependency rules and runtime linker fixups.
11. Add language backends for Zig, Nim, Swift, JVM, and other ecosystems.

## Hermetic Contract

The MVP defines hermeticity at the action boundary:

- Every action has an explicit executable, arguments, environment, inputs, outputs, and working directory.
- The action cache key includes the rustc version, profile, host platform, command arguments, environment, declared input file contents, and output paths.
- Rust actions run with `env_clear`, plus only `LANG`, `LC_ALL`, `TMPDIR`, `TMP`, and `TEMP`.
- Source inputs are discovered from package manifests and package files, excluding build output directories.
- Path dependency outputs are passed explicitly with `--extern`.
- `build.rs` scripts are compiled and run as separate actions with a minimal Cargo-like environment.
- Proc-macro crates are compiled as host dynamic compiler plugins and passed explicitly with `--extern`.
- Fetched source dependencies are stored under `target/tong/store/sources`.
- Build outputs live under `target/tong`.

This is not yet a complete security sandbox. OS-level enforcement is future work:

- Linux: namespaces, read-only bind mounts, seccomp, network namespaces.
- macOS: sandbox profiles or a dedicated sandbox launcher.
- Windows: Job Objects, restricted tokens, ACL-isolated directories, and explicit DLL closure handling.

The intended long-term model is that all language rules lower into the same action abstraction, so independent compile units can be cached more precisely than Cargo's package-level build model.

More detail:

- [Roadmap](docs/ROADMAP.md)
- [Hermeticity Model](docs/HERMETICITY.md)
