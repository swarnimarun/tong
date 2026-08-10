# Usage Guide

All commands run from the workspace root. `tong --help` lists the
subcommands; `tong <command> --help` documents each command's flags.

```text
Usage: tong <COMMAND>

Commands:
  build       Build the workspace
  run         Build and run a binary target
  clean       Remove the project-local `.tong` directory
  test        Build and run the test targets
  lock        Resolve versions and write `Tong.lock`
  fetch       Download locked crate sources into the store
  update      Re-resolve `Tong.lock` (optionally for one package)
  toolchain   Manage toolchains
  dockerfile  Generate a layer-cache-friendly Dockerfile and .dockerignore
  gc          Garbage-collect the store: delete unreferenced cache objects
```

## Common concepts

**Labels.** Targets are addressed by label: `:hello` (same-package),
`hello` (bare — dashes and underscores are interchangeable, so
`:voxel_city` finds the Cargo package `voxel-city`), or `//path:name`
(parsed for compatibility). `tong run` and `tong test` take labels;
`tong build` with positional labels restricts *materialized* artifacts
to the listed targets (the whole graph still builds). `--target` is
exclusively a Rust target triple, like Cargo's.

**Profiles.** `dev` is the default, `release` exists by default; further
profiles are declared in `Tong.toml` (`[profile.<name>]`) or imported
from `Cargo.toml` (`[profile.<name>]`). Select with `--profile`.

**Features.** `--features a,b` activates features on the selected
workspace packages (selection = `--target` labels, or all members);
`--no-default-features` and `--all-features` behave as in Cargo. In
native mode, `Tong.toml` target-level `features`/`default_features` also
apply.

**Output.** Every build prints a summary:

```text
build complete: 3 actions (2 cached, 1 executed)
artifact: .tong/out/dev/app/app
```

Executed actions are cached per digest, so no-change builds print
`N actions (N cached, 0 executed)` and run in tens of milliseconds.

## Build

```sh
tong build [--profile <name>] [--target <label>]...
           [--features <list>] [--no-default-features] [--all-features]
           [--deps-only]
```

- `--deps-only` executes only actions owned by non-workspace packages
  (registry and external path dependencies); workspace actions are
  skipped and nothing is assembled. This is the Docker dep-layer
  primitive — see [Docker Caching](docker.html).
- Artifacts are materialized under `.tong/out/<profile>/<target>/`.
- A missing `Tong.lock` is not an error: Tong resolves and writes it
  first, printing `tong: no Tong.lock — running 'tong lock' first`
  (Cargo generates `Cargo.lock` the same way). Missing locked sources
  are fetched on demand. Explicit `tong lock`/`tong fetch` are the
  recommended repo-side steps.

## Run

```sh
tong run <label> [--profile <name>] [--features <list>]
                 [--no-default-features] [--all-features] [-- args...]
```

Builds the target and executes it. Arguments after `--` are passed to
the program. The process exit code is propagated:

```sh
tong run :voxel_city -- --frames 120
```

## Test

```sh
tong test [label] [--profile <name>] [--features <list>]
                 [--no-default-features] [--all-features] [-- args...]
```

Builds and runs test targets: `[[test]]`/`[[bench]]` declarations, the
auto-derived lib unit test, and native `rust_test` targets. Without a
label, every test target runs; the exit code is 0 when every suite
passed. A label selects by test name, package name (all of that
package's tests), or `pkg:name`. Arguments after `--` go to the test
binaries (libtest flags such as `--nocapture` work).

## Clean

```sh
tong clean
```

Removes the project-local `.tong` directory. In shared-store mode
(`TONG_STORE_DIR` or `[store] dir`), it removes only this project's exec
roots, outputs, and state manifests, then sweeps store objects no other
project's manifest references — it never touches another project's live
objects. The per-machine toolchain capture cache (`~/.cache/tong`) is
not touched either.

## Lock, fetch, update — dependency lifecycle

```sh
tong lock [--offline]
tong fetch [--offline]
tong update [package]
```

- `tong lock` resolves versions against the registry index (default
  crates.io sparse; see `[registry]`/`TONG_REGISTRY_INDEX`), preferring
  the existing `Tong.lock`, and writes `Tong.lock`. `--offline` uses
  only the cached index and fails when an entry is missing.
- `tong fetch` downloads every locked crate into the source store. It
  is a no-op when everything is already stored; `--offline` fails on
  anything missing. `tong fetch --offline` is *not* the Docker builder
  flag — the first fetch needs network.
- `tong update` re-resolves the lockfile; `tong update <package>` drops
  only that package's lockfile preference (Cargo `update -p`
  semantics).

Commit `Tong.lock` (like `Cargo.lock`). Builds resolve registry
dependencies exclusively through the lockfile and the source store —
**normal builds never touch the network**.

## Toolchain

```sh
tong toolchain fetch rust --version <ver> [--target <triple>]
```

Downloads a pinned Rust toolchain bundle (rustup dist protocol) into the
store. Pair it with a manifest:

```toml
[toolchain.rust]
kind = "dist"
version = "1.90.0"
```

A pinned dist toolchain is portable and skips system capture. The
default `kind = "system"` captures the local rustc, its sysroot, and
linker configuration, content-fingerprints the closure into the store
(verified once, then cached per-machine at `~/.cache/tong`), and marks
the capture non-portable — see [Caching and the Store](caching.html).

## Dockerfile

```sh
tong dockerfile [--profile <name>] [--base <image>]
                [--runtime-base <image>] [--output <dir>]
```

Generates a layer-cache-friendly `Dockerfile` + `.dockerignore` for the
workspace. See [Docker Caching](docker.html).

## Garbage collection

```sh
tong gc [--older-than <duration>] [--max-size <size>] [--dry-run]
```

Deletes unreferenced store objects. Defaults come from
`[store] retention` / `[store] max_size` (or `TONG_STORE_RETENTION` /
`TONG_STORE_MAX_SIZE`): retention floor `7d`, size budget `10G`.
`--older-than 0` deletes all unmarked objects immediately; `--dry-run`
reports without deleting. GC also runs automatically after every build
(best-effort). See [Caching and the Store](caching.html).

## Environment variables

| Variable | Meaning | Default |
|---|---|---|
| `TONG_STORE_DIR` | Content-addressed store location (shared-store mode) | `<root>/.tong/store` |
| `TONG_STORE_RETENTION` | Auto-GC age floor, human format (`7d`, `30d`) | `7d` |
| `TONG_STORE_MAX_SIZE` | Store size budget (`10G`, `500M`) | `10G` |
| `TONG_CACHE_DIR` | Per-machine toolchain capture cache | `~/.cache/tong` |
| `TONG_REGISTRY_INDEX` | Registry index URL (`sparse+https://…`, `https://…`, `file://…`) | crates.io sparse |
| `RUST_LOG` | Tracing filter; `tong::perf=debug` emits per-phase perf events to stderr | `warn` |

Precedence: `TONG_STORE_DIR` wins over `[store] dir` in `Tong.toml`
(native mode only), which wins over the project-local default.
`TONG_REGISTRY_INDEX` wins over `[registry] index`.

Example — forward perf events without polluting stdout:

```sh
RUST_LOG=tong::perf=debug tong build 2> perf.log
```

## Workspace layout

```text
<root>/
  Tong.toml | Cargo.toml   # mode selector
  Tong.lock                # committed dependency lockfile (Cargo mode)
  .tong/
    store/                 # content-addressed store (blobs, trees,
                           # actions, results, sources, toolchains,
                           # bundles, state)
    out/<profile>/<target>/  # materialized artifacts
    …                      # internal exec roots, project state
```

`Tong.toml` is optional; its absence means Cargo import mode.
