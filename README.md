# Tong

Tong is a declarative, hermetic, multi-language build system for monorepos.
It preserves familiar language workflows while lowering all build work into
explicit, cacheable actions.

Status: **experimental (0.1.0)**. Rust is the first certified backend; the
Rust, C, and C++ coexistence goal and the remaining platform certification
work are tracked in the [roadmap](https://swarnimarun.github.io/tong/roadmap.html).

## What makes Tong different

- **Actions are the only execution unit.** No backend ever invokes Cargo,
  CMake, or a compiler driver as an ambient subprocess during analysis.
  Everything — compilation, linking, build scripts, proc macros, tests,
  fetching — is an explicit action with declared inputs, outputs,
  toolchain, environment, and sandbox policy.
- **Analysis is pure.** Given the manifests, lockfile, platform, toolchain
  metadata, and selected configuration, analysis produces the same action
  graph every time: no ambient file reads, no network, no time or RNG.
- **Hermeticity and reproducibility are separate claims**, reported
  independently, with enforcement levels from clean environments (L1) up
  to filesystem isolation and network denial (L3/L4).
- **Toolchains are dependencies.** A compiler is not a host property: it is
  an input closure (executables, sysroot, SDK files, linker config) with a
  stable content fingerprint. System rustc is captured and fingerprinted
  into the store; pinned dist toolchains can be downloaded explicitly.
- **Content-addressed everything.** Actions, trees, blobs, results, and
  sources live in one CAS store; action digests cover only semantic
  execution fields, so renames, relocations, and equivalent graphs all
  reuse cache entries. No-op rebuilds take ~40 ms.
- **No embedded build language.** Targets are data in `Tong.toml`
  (native mode) or plain `Cargo.toml` workspaces (import mode). No
  Starlark, no scripting language to learn.

## Quick start

Build the CLI:

```sh
cargo build --release
# binary at target/release/tong
```

Native mode — a `Tong.toml` workspace:

```sh
mkdir hello && cd hello
cat > Tong.toml <<'EOF'
[workspace]
name = "hello"

[toolchain.rust]
kind = "system"

[target.hello]
rule = "rust_binary"
crate_root = "src/main.rs"
edition = "2021"
EOF
mkdir src
cat > src/main.rs <<'EOF'
fn main() { println!("hello from tong"); }
EOF
tong build          # artifacts land in .tong/out/dev/hello/hello
tong run :hello
```

Cargo import mode — an existing Cargo workspace works as-is:

```sh
cd examples/01-calc
tong build
tong run :calc_cli
tong test
```

No `Cargo.toml`? No problem. No `Tong.toml`? Also fine. Tong picks the
mode from whichever manifest is present.

## Feature highlights

| Area | What works today |
|---|---|
| Manifests | Native `Tong.toml` targets (`rust_library`, `rust_binary`, `rust_proc_macro`, `rust_test`, `cc_import`) and full Cargo-workspace import |
| Rust backend | Libraries, binaries, proc macros, tests, build scripts (`cargo:` directives), cdylib/staticlib/dylib, features, workspace inheritance, target-specific deps |
| Native interop | `cc_import` for prebuilt C libraries (e.g. SDL3), build-script link directives propagated to dependents |
| Sources | `Tong.lock` + `tong lock` / `tong fetch` / `tong update`; crates.io sparse index; fully offline builds |
| Toolchains | System capture (content-fingerprinted, snapshot-cached) or pinned dist bundles via `tong toolchain fetch rust --version` |
| Caching | Content-addressed store, per-action cache, build-state manifests, reachability GC (`tong gc`), shared stores (`TONG_STORE_DIR`) |
| Sandboxing | Opt-in `[policy] sandbox` levels l1–l4 (bubblewrap on Linux, Seatbelt on macOS, clean-env on Windows) |
| Docker | `tong build --deps-only` and `tong dockerfile` for layer-cache-friendly images |
| Determinism | SHA-256 canonical digests, pure analysis, clean action environments, no ambient cargo |

## Examples

| Example | Mode | Shows |
|---|---|---|
| `01-hello` | Tong.toml | Minimal native binary |
| `01-calc` | Cargo | Lib + bin workspace, `tong test` |
| `02-advanced` | Cargo | Build script, cdylib + rlib, 6-action graph |
| `03-sdl3` | Tong.toml | `cc_import` of a prebuilt C library |
| `04-voxel-city` | Tong.toml | External crate roots, real game loop |
| `05-voxel-city-cargo` | Cargo | External path deps + build-script link directives |
| `06-sdl3-cargo` | Cargo | SDL3 bindings via `cargo:` directives |
| `07-web-app` | Cargo | Registry deps (axum + tokio) via `Tong.lock` |

## Documentation

The full documentation lives in the [Tong Book](https://swarnimarun.github.io/tong/):

- [Quick start](https://swarnimarun.github.io/tong/quick-start.html)
- [Usage guide / CLI reference](https://swarnimarun.github.io/tong/usage.html)
- [Manifest reference](https://swarnimarun.github.io/tong/manifest-reference.html)
- [Cargo import mode](https://swarnimarun.github.io/tong/cargo-import.html)
- [Hermeticity model](https://swarnimarun.github.io/tong/hermeticity.html)
- [Caching and the store](https://swarnimarun.github.io/tong/caching.html)
- [Docker caching](https://swarnimarun.github.io/tong/docker.html)
- [Architecture](https://swarnimarun.github.io/tong/architecture.html)
- [Roadmap](https://swarnimarun.github.io/tong/roadmap.html)

Build the book locally:

```sh
mdbook build book   # output in book/book
mdbook serve book   # live preview at http://localhost:3000
```

Design documents that drive the implementation live in [`docs/`](docs/):
[docker-caching.md](docs/docker-caching.md), [fingerprint-cache.md](docs/fingerprint-cache.md),
[performance.md](docs/performance.md), and the governing
[`PLAN.md`](PLAN.md).

## Development

```sh
just setup    # install pinned toolchain components (idempotent)
just build    # cargo build --workspace
just check    # fast type-check
just test     # cargo test --workspace
just lint     # clippy with warnings denied
just fmt      # cargo fmt --all (check: just fmt --check)
just run -- … # run the tong CLI with args
```

CI runs the same gates (`fmt --check`, `clippy -D warnings`, full test
matrix on Linux/macOS/Windows) plus an mdBook build on every push and PR;
the book is deployed to GitHub Pages on every push to `main`.

## License

MIT OR Apache-2.0.
