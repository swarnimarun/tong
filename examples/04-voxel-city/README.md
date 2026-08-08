# Voxel city builder (SDL3 + Tong example)

A tiny isometric voxel city builder: a procedural seed city rendered with
a painter's-algorithm 2:1 isometric projection into a streaming texture.
Built on the shared `sdl3-sys` bindings from `examples/03-sdl3`, which
this manifest imports as an **external crate root** (`..` path — Tong
mounts the crate's directory into the exec root at plan time).

## Prerequisites

- SDL3 via Homebrew: `brew install sdl3`
- The `[target.sdl3]` manifest entry points at the brew prefix
  (`/opt/homebrew/opt/sdl3/...`); adjust for other prefixes.

## Build and run

```sh
tong build
tong run :voxel_city            # interactive
SDL_VIDEODRIVER=dummy tong run :voxel_city -- --frames 120   # headless
tong run :voxel_city -- --frames 2 --screenshot out.ppm      # write frame 1
```

## Controls

- Left click: place a block (brush) on the hovered column
- Right click: remove the top block of the hovered column
- Keys `1`-`7`: brush (grass, dirt, stone, wood, leaf, water, brick)
- `ESC` or window close: quit

## What it shows

- A shared bindings crate consumed across example workspaces: `Tong.toml`
  targets may reference crate roots outside the package directory; the
  backend captures and mounts them (`ext/<n>`) instead of requiring a copy.
- A real game loop on the bindings: `SDL_CreateTexture` + `SDL_UpdateTexture`
  streaming backbuffer, `SDL_PollEvent` keyboard/mouse handling, and
  `SDL_GetTicks`-based pacing.
- A from-scratch iso renderer: painter's order over columns, per-face
  visibility against neighbor column heights, hover picking via the
  inverse projection, and a deterministic (hash-seeded) seed city.
