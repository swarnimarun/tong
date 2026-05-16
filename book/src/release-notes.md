# Release Notes

## 0.1.1

- Added `tong run` to build and run binary targets in one step.
- `tong run --bin NAME` selects a binary when a package has multiple binary
  targets.
- Arguments after `--` are forwarded to the selected binary.
- Internal cleanup split large implementation files into smaller private
  modules without changing public crate APIs.
- CI now validates tests and smoke builds across Linux, macOS, and Windows.

## 0.1.0

Tong 0.1.0 is the first experimental MVP release.

It includes:

- CLI commands for build, fetch, plan, add, and clean.
- Manifest discovery for `Tong.toml` and `Cargo.toml`.
- Basic package, library, binary, feature, and path dependency support.
- Git, tar, `.crate`, and zip dependency source materialization.
- `tong.sources.*` overrides for registry-style transitive dependencies.
- Direct `rustc` compilation for Rust libraries and binaries.
- Basic `build.rs` compile/run support.
- Basic proc-macro crate support.
- Per-action cache keys and clean action environments.
- A language backend boundary for future ecosystem support.

Known limitations:

- No crates.io resolver yet.
- No native dependency fetching yet.
- No rpath, install-name, or DLL fixups yet.
- No OS-level sandbox yet.
- No remote cache yet.
