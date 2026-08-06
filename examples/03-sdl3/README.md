# SDL3 + Tong example

Demonstrates importing a prebuilt C library (`cc_import`) into Rust with a
hand-written bindings layer, built by Tong.

## Prerequisites

- SDL3 via Homebrew: `brew install sdl3`
- The `[target.sdl3]` manifest entry points at the brew prefix
  (`/opt/homebrew/opt/sdl3/...`); adjust for other prefixes.

## Build and run

```sh
tong build
tong run :sdl3_demo          # opens a 640x480 window for 1.5s
SDL_VIDEODRIVER=dummy .tong/out/dev/sdl3_demo/sdl3_demo   # headless
```

## What it shows

- `cc_import`: the prebuilt `libSDL3.0.dylib` is imported into the CAS at
  plan time; dependent Rust crates get `-L native` and `-l SDL3.0`.
- `sdl3_sys`: hand-written FFI bindings (`extern "C"`).
- `sdl3_demo`: links the bindings; the final artifact directory contains
  the binary plus the SDL3 runtime library (runtime closure, PLAN.md
  section 13).
