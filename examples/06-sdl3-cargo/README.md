# SDL3 bindings — plain Cargo package (SDL3 + Tong example)

The shared SDL3 FFI bindings as a normal Cargo package (the "SDL3 via
Cargo" counterpart of `examples/03-sdl3`, which uses a `Tong.toml`
`cc_import` instead). The native library is linked through the build
script's `cargo:` directives — Cargo's mechanism for prebuilt
libraries — which tong parses from the build-script run and applies to
every crate that depends on the package, mirroring Cargo.

Consumed as an external path dependency by `examples/05-voxel-city-cargo`.

## Prerequisites

- SDL3 via Homebrew: `brew install sdl3` (or set `SDL3_DIR` to the prefix)
- The default prefix is `/opt/homebrew/opt/sdl3`; the search path is a
  host path — non-portable by design, like tong's system toolchain
  capture, and the runtime library loads through its absolute install name.

## Build

```sh
# with cargo
cargo build

# with tong (imports the workspace the same way)
tong build
```

## What it shows

- The build-script directive mechanism shared by both tools:
  `cargo:rustc-link-search` / `cargo:rustc-link-lib` from `build.rs`.
- A library-only workspace that both `cargo` and `tong` import
  identically; run the game that consumes it in `examples/05-voxel-city-cargo`.
