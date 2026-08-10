# Tong 0.2.0 (draft)

Tong is a declarative, hermetic, multi-language build system for monorepos.
Everything — compilation, linking, build scripts, proc macros, tests — is
lowered into explicit, cacheable actions, so builds stay reproducible and
replayable.

This release is not ready to tag. It is blocked until the required external
Cargo corpus passes both resolver and offline-build gates and the platform,
packaging, and release checks are green. The current implementation adds the
foundations below, but Cargo compatibility remains incomplete.

## Highlights

- **Cargo-import Rust backend.** Import `Cargo.toml` manifests directly —
  workspace-inherited fields, workspaces, path and external deps, feature
  resolution (resolvers v2/v3), Cargo profile parity (`debug-assertions`, `strip`,
  `rpath`), target-specific deps via `cfg` evaluation, `build = false`
  opt-outs, build scripts, proc macros, and `cc_import` libraries. A
  differential compatibility suite (`tong-rust/tests/compat`) checks the
  planned action graph against real Cargo runs, with a pinned external Rust
  corpus. The required corpus is a release gate, not yet a completed claim.
- **Registry and dependency management.** crates.io sparse-index resolution,
  `Tong.lock` locking, pinned git sources, and `lock` / `fetch` / `update`
  subcommands. `--offline`, `--locked`, and `--frozen` build modes enforce
  offline and lockfile policies with targeted diagnostics.
- **Sandboxed execution.** Linux builds can run under bubblewrap and macOS
  under Seatbelt, with a clean-environment fallback; policy is configurable.
  Execution roots are deterministic and environments are scrubbed.
- **Content-addressed store with GC.** Reachability-based mark-and-sweep
  garbage collection, automatic post-build GC, build-state manifests, and
  immutable blobs keep the store lean without breaking cache correctness.
- **Toolchain management.** `tong toolchain fetch` downloads rustup-dist
  component bundles; toolchain capture is cached so analysis stays fast.
- **CLI.** `build`, `check`, `run`, `test`, `bench`, `clean`, `lock`,
  `fetch`, `update`, `query`, `graph`, `explain`, `log`, `dockerfile`, and
  `gc`, plus Cargo-style workflow commands. Native target labels
  (`:name`, `//member:name`) select targets in `Tong.toml` mode.
- **Docker layer caching.** `tong dockerfile` generates a layer-cache-friendly
  multi-stage `Dockerfile` and `.dockerignore`; the web example demonstrates
  the full docker-cached build flow.

## Examples

Runnable workspaces now cover the whole pipeline:

- `01-hello`, `01-calc` — native `Tong.toml` and Cargo-imported projects
- `02-advanced` — build scripts and cdylibs
- `03-sdl3`, `04-voxel-city`, `05-voxel-city-cargo`, `06-sdl3-cargo` —
  SDL3 bindings and voxel-city demos, in native and plain-Cargo flavors
- `07-web-app` — registry workflow (`lock` / `fetch` / offline `build`) with
  Docker layer caching

## Docs

- New mdBook site (`book/`) with build and testing instructions
- Guides for store GC, Docker layer caching, and performance
