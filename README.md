# Tong

Status: experimental, vibe-coded, and not yet ready for production use.

AI assisted in writing this code.

Tong is an experimental Rust-first hermetic build system. The current MVP can
build simple Rust libraries and binaries from `Cargo.toml` or `Tong.toml`
without depending on Cargo's build execution model.

## Quick Start

```sh
cargo build -p tong
cargo run -p tong -- build examples/simple-rust-project
./examples/simple-rust-project/target/tong/debug/bin/hello-tong
cargo run -p tong -- run examples/simple-rust-project
```

Inspect the package graph:

```sh
cargo run -p tong -- plan examples/simple-rust-project
```

Build examples with Tong manifests, path dependencies, and build scripts:

```sh
cargo run -p tong -- build examples/tong-manifest-project
cargo run -p tong -- build examples/path-dep-project
cargo run -p tong -- build examples/build-script-project
```

## Documentation

The project book is published at
[swarnimarun.github.io/tong](https://swarnimarun.github.io/tong/).

Local docs can be built with:

```sh
mdbook build book
```

## Status

Tong 0.1.1 is the current MVP release. It supports explicit Rust compile actions, source
materialization from fixed sources, basic build scripts, proc-macro crates, and
per-action cache keys. It does not yet implement crates.io resolution, native
dependency fetching, OS-level sandboxing, or remote caching.

See [CHANGELOG.md](CHANGELOG.md) and [docs/ROADMAP.md](docs/ROADMAP.md) for the
current release notes and roadmap.

## License

Tong is licensed under the [MIT License](LICENSE).
