# Roadmap

Tong's near-term roadmap is focused on making the Rust MVP useful enough to
exercise real projects while keeping the action model stable.

## Cleanup And Testability

Keep public crate APIs steady while splitting large implementation files into
private modules. Pure pieces should stay directly testable: manifest parsing,
dependency parsing, feature resolution, source selection, rustc arguments,
build script output parsing, and action cache keys.

`tong-core` remains the shared interface layer. A separate traits crate should
only be added if a real dependency cycle appears.

## Cargo Compatibility

This is the next product priority:

1. Improve manifest diagnostics with section/key context.
2. Expand feature resolution toward Cargo-compatible unification.
3. Add test and example targets.
4. Expand `build.rs` support for build-dependencies, rerun directives,
   generated inputs, and more `cargo:`/`cargo::` output forms.
5. Use compiler dep-info where available to improve input discovery.

## Fetching And Lockfiles

After the compatibility layer is sturdier, Tong should add `Tong.lock`, resolved
source metadata, stronger checksum handling, content-addressed source storage,
and explicit network access for fetch/update commands. crates.io resolution
should build on that lockfile model.

## Platform Correctness

Cross-platform work should proceed in layers:

1. Validate tests and smoke builds on Linux, macOS, and Windows in CI.
2. Add runtime-link handling for Linux `RUNPATH`, macOS install names/`LC_RPATH`,
   and Windows DLL closure.
3. Add optional sandbox launchers for Linux namespaces/seccomp, macOS sandbox
   profiles, and Windows Job Objects/restricted tokens.

## Future Backends

Additional language backends should lower into the same action abstraction and
reuse the shared cache, store, sandbox, and query layers. Zig or Nim are likely
better early candidates than JVM ecosystems because their compilation models are
closer to Rust.
