# Roadmap

Tong's near-term roadmap is focused on making the Rust MVP useful enough to
exercise real projects.

1. Improve manifest parsing and diagnostics.
2. Add crates.io resolution.
3. Add native dependency rules.
4. Add runtime linker fixups for rpath, install-name, and DLL closure handling.
5. Add OS-level sandboxing.
6. Add remote cache support.
7. Expand language backends beyond Rust.

The long-term model is that all language rules lower into the same action
abstraction, so independent compile units can be cached more precisely than
Cargo's package-level build model.
