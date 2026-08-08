# Voxel city builder — self-contained Cargo edition (SDL3 + Tong example)

The same demo as `examples/04-voxel-city`, but as a **plain Cargo
workspace with no `Tong.toml`**: the SDL3 bindings (`sdl3-sys`) and the
game (`voxel-city`) are both workspace members here. The exact same
manifests compile and run with both `cargo` and `tong build`.
(`examples/06-sdl3-cargo` is the variant that imports the shared
bindings crate from 03-sdl3 as an external path dependency instead of
carrying a copy.)

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
SDL_VIDEODRIVER=dummy tong run :voxel-city -- --frames 120
```

## Controls

- Left click: place the brush on the hovered column; grass/dirt/stone/
  brick/wood/leaf stack, water and road carve the column flat
- Right click: remove the top block of the hovered column
- Keys `1`-`8`: brush (grass, dirt, stone, brick, wood, leaf, water, road)
- `ESC` or window close: quit

## What it shows

- A fully self-contained Cargo workspace: build scripts, native linking
  via directives, and a multi-crate layout that both `cargo` and `tong`
  build and run identically from the same `Cargo.toml` files.
