# Changelog

All notable changes to Tong are documented in this file.

## 0.1.1 - CLI Run And Cleanup

### Added

- `tong run` builds and runs a binary target, with `--bin NAME` for package
  manifests that define multiple binaries and `--` passthrough for program
  arguments.

### Changed

- Split large internal modules into smaller private modules while preserving
  public crate APIs.
- Expanded PR CI to validate tests and smoke builds across Linux, macOS, and
  Windows.

### Fixed

- Corrected the default linker selection for Rust MSVC builds on Windows.
- Fixed Linux CI issues on Ubuntu and a Windows proc-macro regression.

## 0.1.0 - Initial MVP

Tong 0.1.0 is the first experimental release of a Rust-first hermetic build
system. It focuses on making simple Rust packages build through explicit
actions rather than Cargo's build execution model.

### Added

- CLI commands: `tong build`, `tong fetch`, `tong plan`, `tong add`, and
  `tong clean`.
- Manifest discovery for `Tong.toml` and `Cargo.toml`.
- Basic `[package]`, `[lib]`, `[[bin]]`, `[features]`, and path dependency
  parsing.
- Git, tar, `.crate`, and zip dependency source materialization.
- `tong.sources.*` overrides for registry-style transitive dependencies.
- Direct `rustc` compilation for Rust libraries and binaries.
- Basic `build.rs` compile/run support.
- Basic proc-macro crate support.
- Per-action cache keys based on tools, args, environment, inputs, outputs,
  profile, and platform.
- Clean action environments via `env_clear`.
- Explicit action inputs and outputs.
- A language backend boundary intended for future Zig, Nim, Swift, JVM, and
  other rules.

### Known Limitations

- crates.io dependency resolution is not implemented.
- Native dependency fetching is not implemented.
- Runtime linker fixups such as rpath, install-name, and DLL closure handling
  are not implemented.
- OS-level sandboxing is future work.
- Remote caching is future work.
