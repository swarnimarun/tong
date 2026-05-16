# Tong Roadmap

Tong should grow through explicit layers. Each layer must keep the action
boundary stable so Rust, Zig, Nim, Swift, JVM, and future backends can share the
same scheduler and cache.

## Current Foundation

- Five-crate workspace: CLI, core action primitives, manifest parsing, package
  graph/source materialization, and Rust backend.
- mdBook documentation and MIT licensing.
- Unit tests around cache keys, manifest parsing, feature propagation, CLI
  options, build script output parsing, and rustc argument construction.
- CI should validate formatting and linting on Linux, then run tests and smoke
  builds on Linux, macOS, and Windows.

## Phase 1: Cleanup And Testability

- Keep public crate APIs stable while splitting large implementation files into
  private modules.
- Keep pure units small enough to test directly: manifest values, dependencies,
  feature resolution, dependency source selection, rustc args, build script
  parsing, and action cache keys.
- Avoid adding a dedicated traits crate until an actual cycle appears. For now,
  `tong-core` remains the shared interface layer.

## Phase 2: Cargo Compatibility

- Improve `Cargo.toml` and `Tong.toml` diagnostics with section/key context.
- Expand feature resolution toward Cargo-compatible unification.
- Add test targets, example targets, and target-specific metadata.
- Expand `build.rs` support with build-dependencies, rerun directives,
  generated inputs, and additional `cargo:`/`cargo::` output forms.
- Use compiler dep-info where available to improve declared input discovery.

## Phase 3: Fixed-Output Fetching And Lockfiles

- Add `Tong.lock` with resolved source identity, checksums, and source metadata.
- Make network access explicit to `tong fetch` or future update-style commands.
- Harden git, archive, and local source handling.
- Store sources in a content-addressed layout.
- Add crates.io source resolution after the lockfile format exists.

## Phase 4: Native Dependencies

- Add native package manifests.
- Build native dependencies in isolated actions.
- Expose include, lib, bin, and pkg-config providers.
- Prevent ambient system library discovery.
- Add CMake/configure/make builders as rules rather than hardcoded behavior.

## Phase 5: Runtime Linking

- Linux: pass and patch `RUNPATH`.
- macOS: patch install names and `LC_RPATH`.
- Windows: compute DLL runtime closure and choose copy or launcher strategies.

## Phase 6: Sandboxing

- Add sandbox launchers behind a shared interface.
- Linux: namespaces, read-only mounts, seccomp, and network namespaces.
- macOS: sandbox profiles or a launcher process.
- Windows: Job Objects, restricted tokens, and ACL-isolated directories.
- Keep sandboxing optional until each platform path has reliable CI coverage.

## Phase 7: More Language Backends

- Add a `LanguageBackend` implementation per ecosystem.
- Keep frontend-specific behavior isolated to rule lowering.
- Reuse the same action model, cache, store, sandbox, and query layers.
- Start with Zig or Nim before JVM because their compilation models are closer
  to Rust.
