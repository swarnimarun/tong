# Voxel city builder — Cargo edition (SDL3 + Tong example)

The same demo as `examples/04-voxel-city`, but as a **plain Cargo
workspace with no `Tong.toml`** — and its one dependency is an
**external Cargo project**: `sdl3-sys` lives in `examples/06-sdl3-cargo`
and is pulled in as a path dependency outside this workspace. The exact
same manifests compile and run with both `cargo` and `tong build`.

Native SDL3 linking uses Cargo's own mechanism instead of `cc_import`:
the `sdl3-sys` build script emits `cargo:rustc-link-search` /
`cargo:rustc-link-lib` directives. Cargo consumes them natively; tong
parses them from the build-script run and **propagates them to every
crate that depends on the package** — the same Cargo semantics — so the
binary links identically under both tools.

## Prerequisites

- SDL3 via Homebrew: `brew install sdl3` (or set `SDL3_DIR` to the prefix)
- The default prefix is `/opt/homebrew/opt/sdl3`; the search path is a
  host path — non-portable by design, like tong's system toolchain
  capture, and the runtime library loads through its absolute install name
  (no dylib is shipped alongside the binary).

## Build and run

```sh
# with cargo
cargo build
SDL_VIDEODRIVER=dummy cargo run -p voxel-city -- --frames 120

# with tong
tong build
SDL_VIDEODRIVER=dummy tong run :voxel_city -- --frames 120
```

## Controls

- Left click: place the brush on the hovered column; grass/dirt/stone/
  brick/wood/leaf stack, water and road carve the column flat
- Right click: remove the top block of the hovered column
- Keys `1`-`8`: brush (grass, dirt, stone, brick, wood, leaf, water, road)
- `ESC` or window close: quit

## What it shows

- A Cargo workspace whose dependency is an external Cargo project
  (`path = "../../../06-sdl3-cargo/crates/sdl3-sys"`): tong imports it
  recursively (canonicalized, deduplicated, cycle-checked), builds its
  build script, and propagates its link directives — one shared bindings
  package across the Cargo examples.
- Full Cargo compatibility: workspace-inherited fields, build scripts,
  lib/bin renames, and prebuilt native libraries all build identically
  with `cargo` and `tong`.
