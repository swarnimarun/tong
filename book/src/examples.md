# Examples

Eight runnable workspaces under `examples/` double as the end-to-end
acceptance suite. Each is a complete, self-contained project; `tong
build` from the example directory is the canonical invocation.

| Example | Mode | What it shows |
|---|---|---|
| `01-hello` | Tong.toml | Minimal native binary |
| `01-calc` | Cargo | Lib + bin workspace; `tong test` |
| `02-advanced` | Cargo | Build script, cdylib + rlib, 6-action graph |
| `03-sdl3` | Tong.toml | `cc_import` of a prebuilt C library |
| `04-voxel-city` | Tong.toml | External crate roots, real game loop |
| `05-voxel-city-cargo` | Cargo | External path deps + build-script link directives |
| `06-sdl3-cargo` | Cargo | SDL3 bindings via `cargo:` directives |
| `07-web-app` | Cargo | Registry deps (axum + tokio) via `Tong.lock` |

## 01-hello — minimal native binary

```sh
cd examples/01-hello
tong build
tong run :hello
```

A single `rust_binary` target in a `Tong.toml`; the smallest complete
workspace.

## 01-calc — Cargo lib + bin

```sh
cd examples/01-calc
tong build
tong run :calc_cli
tong test
```

A two-crate Cargo workspace (`calc-core` lib, `calc-cli` bin) built
identically by Cargo and Tong. Exercises the dual-tool compatibility
and the test pipeline.

## 02-advanced — build script + cdylib

```sh
cd examples/02-advanced
tong build
tong run :advanced_app
```

Three crates: `advanced-core` (cdylib + rlib), `shout`, and
`advanced-app`, with a workspace build script and profile settings
(`overflow-checks`, thin LTO in release). Six actions: build script +
script run, lib, cdylib, second lib, bin, link.

## 03-sdl3 — prebuilt C library into Rust

Prerequisite: `brew install sdl3` (the manifest points at the brew
prefix).

```sh
cd examples/03-sdl3
tong build
tong run :sdl3_demo          # opens a 640x480 window for 1.5s
SDL_VIDEODRIVER=dummy .tong/out/dev/sdl3_demo/sdl3_demo  # headless
```

- `cc_import`: the prebuilt `libSDL3.0.dylib` is imported into the CAS
  at plan time; dependent crates get `-L native -l SDL3.0`.
- `sdl3_sys`: hand-written FFI bindings.
- The final artifact directory contains the binary **plus** the SDL3
  runtime library (runtime closure).

## 04-voxel-city — external crate roots + game loop

Prerequisite: SDL3 via Homebrew, as above.

```sh
cd examples/04-voxel-city
tong build
tong run :voxel_city                       # interactive
SDL_VIDEODRIVER=dummy tong run :voxel_city -- --frames 120   # headless
SDL_VIDEODRIVER=dummy tong run :voxel_city -- --frames 2 --screenshot out.ppm
```

A tiny isometric voxel city builder (deterministic seed town, painter's
algorithm renderer, per-pixel surface textures). The shared
`sdl3-sys` bindings from `examples/03-sdl3` are referenced as an
**external crate root** (`../03-sdl3/...` — Tong mounts the crate's
directory into the exec root at plan time instead of copying it).
Controls: left click places the brush, right click removes, keys `1`–`8`
select brush, `ESC` quits.

## 05-voxel-city-cargo — Cargo edition with external path dep

The same demo as 04, as a **plain Cargo workspace** with one external
path dependency (`sdl3-sys` from `examples/06-sdl3-cargo`). The exact
same manifests compile with both `cargo` and `tong`. Native linking
uses Cargo's own mechanism — the `sdl3-sys` build script emits
`cargo:rustc-link-search` / `cargo:rustc-link-lib`, which tong parses
and **propagates to every crate that depends on the package**.

```sh
cd examples/05-voxel-city-cargo
tong build
SDL_VIDEODRIVER=dummy tong run :voxel_city -- --frames 120
```

## 06-sdl3-cargo — SDL3 bindings as a plain Cargo package

The `cc_import` counterpart of 03 in Cargo form: the native library is
linked through the build script's `cargo:` directives. Consumed as an
external path dependency by 05.

```sh
cd examples/06-sdl3-cargo
tong build
```

## 07-web-app — registry dependencies

```sh
cd examples/07-web-app
tong lock      # resolve axum + tokio against the crates.io index
tong fetch     # download locked sources into the store
tong build     # fully offline from here
tong run :web_app
```

A two-crate workspace (`web-core` lib, `web-app` bin) depending on
**axum 0.8 and tokio 1** — the registry workflow: `Tong.lock` pins the
activated feature graph, the source store holds the checkouts, and
every later build is offline. This is also the example used by the
docker layer-caching test (`tong/tests/docker_stage.rs`).
