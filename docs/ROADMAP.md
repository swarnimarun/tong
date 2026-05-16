# Tong Roadmap

Tong should grow through explicit layers. Each layer must keep the action boundary stable so Rust, Zig, Nim, Swift, JVM, and other future backends can share the same scheduler and cache.

## Phase 1: Rust MVP

- Parse `Cargo.toml` and `Tong.toml`.
- Discover Rust libraries and binaries.
- Build local path dependency graphs.
- Materialize git, tar, `.crate`, and zip dependency sources.
- Invoke `rustc` directly.
- Cache library and binary compile actions independently.
- Keep action environments explicit and minimal.
- Compile and run simple `build.rs` scripts as separate actions.
- Compile basic proc-macro crates.

Status: implemented for simple Rust projects, local path dependencies, source URL dependencies, `Tong.toml`, simple `build.rs` code generation, proc-macro crates, and a clap-based CLI example.

## Phase 2: Rust Compatibility

- Expand `build.rs` support with build-dependencies and stricter declared inputs.
- Parse more `cargo:` build script output into structured providers.
- Expand proc macro support and host/target separation.
- Expand feature resolution to full Cargo-compatible unification.
- Add test target support.
- Add better module/input discovery from compiler dep-info.

## Phase 3: Fixed-Output Fetching

- Harden `tong fetch`.
- Add `Tong.lock`.
- Support URL, archive, git, and crates.io sources.
- Require hashes for ordinary builds.
- Permit network only in explicit fetch/update actions.
- Store fetched sources in a content-addressed store.

## Phase 4: Native Dependencies

- Add native package manifests.
- Build native dependencies in isolated actions.
- Expose include, lib, bin, and pkg-config providers.
- Prevent ambient system library discovery.
- Add CMake/configure/make builders as rules, not hardcoded behavior.

## Phase 5: Runtime Linking

- Linux: pass and patch `RUNPATH`.
- macOS: patch install names and `LC_RPATH`.
- Windows: compute DLL runtime closure and use copy or launcher strategies.

## Phase 6: More Language Backends

- Add a `LanguageBackend` implementation per ecosystem.
- Keep frontend-specific behavior isolated to rule lowering.
- Reuse the same action model, cache, store, sandbox, and query layers.
- Start with Zig or Nim before JVM because their compilation models are closer to Rust.

## Phase 7: Hard Sandboxing

- Linux: namespaces, read-only mounts, seccomp, network namespaces.
- macOS: sandbox profiles or a launcher process.
- Windows: Job Objects, restricted tokens, ACL-isolated directories.
- Add undeclared input detection where the platform permits it.
