# Introduction

Tong is a declarative, hermetic, multi-language build system for monorepos.
It preserves familiar language workflows while lowering all build work into
explicit, cacheable actions.

```text
Workspace manifests
        ↓
Dependency and configuration resolution
        ↓
Configured target graph
        ↓
Language/backend analysis
        ↓
Immutable action graph
        ↓
Local or remote execution
        ↓
Content-addressed outputs and action results
```

Every backend — Rust today, C/C++ next — lowers targets into the same
immutable action representation. The scheduler, cache, store, sandbox, and
diagnostics have no Rust-specific behavior.

## Why Tong exists

Cargo is excellent at single-crate workflows, but its design couples
compilation, resolution, caching, and build scripts into one opaque
pipeline. Monorepos need:

- **Shared, content-addressed caches** that work across workspaces,
  machines, and CI without trusting mtimes.
- **Explicit hermeticity**: builds that cannot observe undeclared files,
  environment, or the network.
- **Explainable rebuilds**: knowing *why* an action ran is a product
  feature.
- **A multi-language future**: one engine, many declarative frontends.

Tong keeps the ergonomics of the Cargo workflow (manifests, profiles,
features, `--` argument passthrough) while making the execution model
explicit and cacheable.

## Governing principles

1. **Actions are the only execution unit.** Backends may resolve
   packages, inspect manifests, and construct graphs, but every
   compilation, link, code-generation, test, packaging, and tool
   invocation becomes an action. A backend never invokes Cargo, CMake, a
   compiler driver, or another package manager as an ambient subprocess
   during analysis.
2. **Analysis is pure.** Given the same manifests, lockfile, platform
   definitions, configuration, toolchain metadata, and dependency
   provider data, analysis always produces the same graph. It must not
   read undeclared files, touch the network, inspect undeclared host
   environment, run compiler discovery commands, or depend on time,
   randomness, or mutable global state.
3. **Toolchains are dependencies.** A compiler is an input closure:
   executables, runtime and standard libraries, sysroots, linkers,
   SDK files, configuration metadata, and a stable fingerprint. Tong
   distinguishes host, execution, and target platforms; toolchains are
   resolved against platform constraints.
4. **Hermeticity and reproducibility are separate claims.** A hermetic
   action only observes declared inputs and capabilities; a reproducible
   action produces equivalent outputs when re-executed with the same
   inputs. Tong reports both:

   ```text
   Hermeticity: enforced
   Reproducibility: verified
   Cacheability: enabled
   Remote eligibility: enabled
   ```

5. **No embedded build language.** Targets are data in TOML. Extensibility
   comes from built-in declarative rule schemas, versioned backends, and
   a future process- or WASM-based backend protocol — not from a
   Starlark-like language.

## What Tong is not

- A universal package registry
- A replacement compiler
- A programming language for arbitrary build logic
- A replacement for Cargo's publishing, installation, project-generation,
  vendoring, or dependency-editing commands
- A Nix distribution or Nix expression evaluator

Tong also does not promise byte-identical artifacts across target
platforms, and it cannot overcome compiler limitations such as Rust's
crate-level compilation granularity or the absence of a stable Rust ABI.

## Status

Tong 0.2.0-dev is an experimental implementation of the core pipeline plus
the Rust offline backend. Its Cargo-import compatibility is under active
certification; support claims below describe implemented paths, not a promise
that arbitrary Cargo workspaces already pass.

- Native `Tong.toml` targets and Cargo workspace import
- Rust libraries, binaries, proc macros, tests, and build scripts
- `Tong.lock` registry resolution and offline builds
- Content-addressed store, per-action cache, reachability GC
- System toolchain capture with a snapshot-verified per-machine cache,
  and pinned dist toolchains
- Opt-in sandboxing (l1–l4) with per-platform enforcement
- Docker layer caching (`--deps-only`, `tong dockerfile`)

What is planned: C/C++ backend, remote caches and execution, test-result
caching, structured historical logs, and more — see
[Roadmap](roadmap.html).

## Reading this book

- New to Tong? Start with [Quick Start](quick-start.html).
- Reference material: [Usage Guide](usage.html) (CLI and environment),
  [Manifest Reference](manifest-reference.html) (`Tong.toml`),
  [Cargo Import Mode](cargo-import.html).
- Concepts: [Hermeticity Model](hermeticity.html),
  [Caching and the Store](caching.html), [Architecture](architecture.html).
- Practical guides: [Docker Caching](docker.html),
  [Examples](examples.html), [Performance](performance.html).

The governing design document is `PLAN.md` in the repository root; the
`docs/` directory holds focused design records
(`docker-caching.md`, `fingerprint-cache.md`, `performance.md`).
