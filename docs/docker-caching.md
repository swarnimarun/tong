# Docker-friendly caching and build performance features

Tong's per-action content-addressed cache already prevents recompiles
across builds, but a naive `COPY . .` + `RUN tong build` Dockerfile gets
no layer caching: every source edit invalidates the layer holding the
store, so dependency results are lost and re-serialized on the next
build. This document proposes the tong features that make
`docker build` layer-cache-friendly: fetch and dependency compilation
live in layers that bust only when the lockfile or toolchain changes,
and local packages rebuild in a final app layer. It also maps the
tong-relevant items from Ed Page's
[cargo vision post](https://epage.github.io/blog/2026/08/cargo-vision/)
to concrete tong work.

**Scope.** The docker pattern targets Cargo-import workspaces (the mode
with registry dependencies). Native `Tong.toml` mode is offline
path-only today; the sections below mark what applies to it.

**Marking convention.** `[existing]` = works today; `[planned: PLAN.md
§X]` = in the governing plan; `[proposed]` = new in this document.

## How tong caches today

- **Action-level cache.** Each planned action is concretized into an
  `ActionSpec`; its digest covers the canonical semantic fields, the
  input-root tree digest, the environment-bundle digest, and the
  toolchain-closure digests. Logical ids and absolute host paths never
  enter digests — execution roots appear only as
  `{exec_root}`/`{bundle_root}` placeholders substituted by the executor
  (PLAN.md §4.1, §4.4).
- **Build flow.** `prepare` runs store open → model load → feature
  resolution → toolchain capture → plan of the full graph, then a
  schedule loop executes actions in topological order: concretize →
  digest → cache lookup → execute → write result to cache, followed by
  the build-state manifest and auto-GC
  (`tong/src/driver.rs` `build()`).
- **Store.** One content-addressed store with namespaces
  `blobs/trees/actions/results/sources/toolchains/bundles/state`
  (PLAN.md §10); default location `<root>/.tong/store`, relocated via
  `TONG_STORE_DIR` or `[store] dir` in `Tong.toml` (native mode only)
  (`store_dir()` in `tong/src/driver.rs`).
- **Concurrency.** Per-digest atomic writes, no whole-build locks: one
  executor claims a missing action, other clients wait or execute
  independently, completed objects are immutable (PLAN.md §10.1, §10.3).
- **GC.** After each build a build-state manifest per project records
  the full object closure; the latest manifest per project is the GC
  root. Default retention 7d, size budget 10G;
  `tong gc [--older-than <dur>] [--max-size <size>] [--dry-run]` runs a
  manual sweep, configured via `[store] retention`/`[store] max_size` or
  `TONG_STORE_RETENTION`/`TONG_STORE_MAX_SIZE` (PLAN.md §10.4; flags in
  `tong/src/main.rs`, policies in `tong/src/driver.rs`).
- **Version and source flow.** `tong lock` resolves versions against
  the registry index, preferring the existing `Tong.lock` (`--offline`
  uses only the cached index); `tong fetch` downloads locked crates into
  the source store and is a no-op when everything is already stored
  (`--offline` fails on anything missing); builds never touch the
  network (PLAN.md §9; `lock`/`fetch` in `tong/src/driver.rs`).
- **Toolchain.** The system rustc and sysroot are content-fingerprinted
  into the store. The full hash is computed once per toolchain and
  cached per-machine (`ToolchainCache` in `tong-rust`, `$TONG_CACHE_DIR`
  defaulting to `~/.cache/tong`, snapshot-verified), so a warm build
  pays ~0.03 s and a cold capture ~0.28 s (`docs/performance.md`,
  `docs/fingerprint-cache.md`; see Feature 5).

## Why naive docker builds bust the cache

1. `COPY . .` changes with every source edit → the layer containing it
   invalidates on every build.
2. The `.tong/` store (dependency compile results included) lives in the
   same layer as the build → invalidation loses dep results → full
   dependency recompile.
3. Even when results survive, the layer diff containing the dep CAS is
   re-serialized on every push — large layers, slow pushes.

Layer-cache-friendly means separating rarely-changing inputs
(manifests, lockfile, toolchain) from frequently-changing inputs
(sources), with the store flowing forward between stages. This is the
same split cargo users get from `cargo-chef` and the one cargo itself
has been asked for in
[rust-lang/cargo#2644](https://github.com/rust-lang/cargo/issues/2644).

## Feature 1: `tong build --deps-only` `[existing]`

Implemented 2026-08-09 with design (b) below; `--deps-only` is a `tong
build` flag.

**Semantics.** Plan the full graph — feature unification and toolchain
capture unchanged — but execute only actions owned by non-workspace
packages. Local-package actions are skipped entirely: no cache lookup,
no execution, no recording. Nothing is assembled, and state recording +
auto-GC run as usual. Backends tag each planned action with its
ownership (`PlannedAction.external`), so the scheduler needs no id
parsing.

**Local vs dep.** In Cargo-import mode, **local** = workspace members;
**dep** = everything else (registry packages, git sources, paths outside
the workspace). In native `Tong.toml` mode all targets are local, so
`--deps-only` today executes nothing beyond the prepare-phase toolchain
capture; it becomes meaningful when network/fetch rules land (PLAN.md
§9, Phase 3).

**The docker-stage requirement.** The deps stage runs with **only
manifests + lockfile present, no local sources**. This matters because
`RustBackend::plan()` captures every package's source tree at plan time
(`cas.capture_dir_filtered(&pkg.dir, …)`, `tong-rust/src/backend.rs`) —
a missing local package directory fails planning. Implementing
`--deps-only` must therefore either

(a) skip source-tree capture for local packages in this mode, or

(b) have the `tong dockerfile` generator (Feature 3) stage each member
    manifest at its exact relative path, so the directory exists and
    capture succeeds on the manifest-only tree.

The captured tree is never executed in this mode, so its content is
irrelevant. **Design choice: (b)** — no planner changes needed, and the
generator's manifest staging is precisely the input the deps stage
provides.

**CLI shape.** `tong build --deps-only [--profile …] [--features …]`,
orthogonal to `--target` (which still only restricts materialized
artifacts). Not applicable to `test`/`run` — deps-only tests are
meaningless.

**Failure mode.** Missing or stale `Tong.lock` produces the existing
offline/manifest diagnostic, unchanged.

**The app stage** then runs plain `tong build`: dep actions hit the
action cache (input roots unchanged), local actions execute and
materialize under `.tong/out/<profile>/`. Crates with build scripts
re-run their script once at the app stage — the deps stage recorded no
`rerun-if-changed` directives, so the app stage narrows the script's
inputs (PLAN.md §8.6) — after which builds are hits.

**GC safety.** A deps-only build records only dependency actions and
invalidates nothing, so its state manifest merges the previous
manifest's closure (plus every captured package tree) — the merged
manifest stays the GC root, and GC never removes anything the current
workspace references. `tong gc --older-than 0` after a deps-only build
keeps dep results and local trees; a later full build is a total cache
hit (`tong/tests/docker_stage.rs`).

## Feature 2: fetch and lock layers are stable `[existing behavior, pattern]`

`tong lock` and `tong fetch` never read source files — only manifests
and the lockfile — so in docker their `RUN` layer busts exactly when a
manifest or `Tong.lock` changes (docker layer caching keys on the input
filesystem state).

> **Invariant.** Dep layers bust **iff** the lockfile content or the
> toolchain fingerprint changes; source edits touch only the app layer.

- A manifest dependency declaration change → `Tong.lock` changes →
  fetch re-runs (a no-op for already-stored crates) and the deps layer
  rebuilds — correct, because deps actually changed.
- A src-only edit → manifests and `Tong.lock` unchanged → fetch and
  deps layers are cache hits.

**Recommendation.** Commit `Tong.lock` to the repo (like `Cargo.lock`)
so docker never runs `tong lock`; keep `tong lock` a repo-side/CI step.
The requirement is soft: `tong build`/`run`/`test` auto-run `tong lock`
and `tong fetch` (with a notice) when the lock file or a locked source
archive is missing — cargo generates Cargo.lock the same way. Git-dep
sources, once implemented (PLAN.md §9, Phase 3), follow the same layer
logic — they are fetched into the same source store.

**Resolution (2026-08-09).** Version resolution is a port of cargo's
resolver: semver-compatible activation groups (so `syn 2.x` and `3.x`
coexist), DFS with backtrack frames, and a global conflict cache that
backjumps provably-dead states. The lock covers the *activated* feature
graph, like cargo's — for the axum+tokio example, ~50 packages instead
of the ~800 of the full optional closure. `tong lock` prints progress
(`resolved N packages…`, per-crate index fetches); `tong fetch` prints
`downloading i/total`.

One note: `tong fetch --offline` is **not** for docker — the builder
must be allowed network access for the first fetch.

## Feature 3: `tong dockerfile` generator `[existing]`

Implemented 2026-08-09: `tong dockerfile [--profile <name>] [--base
<image>] [--runtime-base <image>] [--output <dir>]`.

**Purpose:** "easily docker build with tong without busting cache." The
generator reads the workspace model and emits the exact Dockerfile +
`.dockerignore`, so no one hand-maintains member-manifest `COPY` lines.

**Flags.** `--profile <name>` (default `dev`), `--base <image>`
(required if the `[toolchain.rust]` version is not pinned; else default
`rust:<version>`), `--runtime-base <image>` (default
`debian:bookworm-slim`), `--output <dir>` (default `.`). Emits
`Dockerfile` and `.dockerignore` into the output dir.

**`.dockerignore`.** `.git/`, `.tong/`, `target/`, `**/target/` — the
local dev store must never be copied over the image's store.

**Template.** Member-manifest `COPY` lines, binary names, and the
version pin are the only generated substitutions (`rust:1.97-bookworm`
and `tong 0.1.0` are examples):

```dockerfile
# syntax=docker/dockerfile:1
# Generated by `tong dockerfile` — do not edit, regenerate.
# Profile: dev

# Stage 0: tong toolchain (busts only when the tong version pin changes).
FROM rust:1.97-bookworm AS toolchain
RUN cargo install tong --version 0.1.0 --locked

# Stage 1: fetch + build deps. Busts only when manifests, Tong.lock, or
# the toolchain change — never on source edits.
FROM toolchain AS deps
WORKDIR /app
COPY Tong.toml Tong.lock ./
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/Cargo.toml
COPY crates/app/Cargo.toml crates/app/Cargo.toml
RUN tong fetch
RUN tong build --deps-only --profile dev

# Stage 2: app. Busts on every source change; dep actions are cache hits.
FROM deps AS app
COPY . .
RUN tong build --profile dev

# Stage 3: runtime.
FROM debian:bookworm-slim AS runtime
COPY --from=app /app/.tong/out/dev/app/app /usr/local/bin/app
ENTRYPOINT ["/usr/local/bin/app"]
```

Notes:

- `Tong.toml`, `Tong.lock`, and `RUN tong fetch` are emitted only when
  the corresponding files exist in the workspace; without a committed
  `Tong.lock` there is nothing to fetch.
- Native-mode projects get `COPY Tong.toml` only — no Cargo lines.
- One `COPY --from=app` line per workspace binary in the runtime stage
  (bin names come from the model; dependency-crate binaries are not
  copied). Artifacts live at `.tong/out/<profile>/<bin>/<bin>`.
- Path dependencies inside the build context are copied in full (their
  actions execute in the deps stage, so their sources are required; a
  source edit busts the deps layer); path deps outside the context are
  reported with a warning and must be copied in or vendored. Registry
  and git checkouts live in the store and arrive via `tong fetch`.
- `COPY . .` in stage 2 never overwrites the stage-1 store because
  `.dockerignore` keeps the local `.tong/` out of the build context.
- The manual equivalent (no generator) is the same template written by
  hand; the generator only removes the bookkeeping.
- The manifests-only deps stage is exactly why Feature 1 chose design
  (b): the generator stages each member manifest at its real relative
  path.

## Feature 4: store placement options `[existing + guidance]`

**Default: keep the store in the image** — the deps layer. Works with
zero config; docker's layer pointers dedupe the store content on push;
recommended for CI.

**Alternative for local iteration:** BuildKit
`--mount=type=cache` + `TONG_STORE_DIR=/tong-store` on the single-`RUN`
pattern. The tradeoff is explicit: cache-mount contents are **not** in
pushed layers, so this does **not** give the deps/app layer split. Use
it only with one `RUN tong build` per stage that needs it, or mount the
same cache id in every stage.

**GC in docker.** Auto-GC after each build is fine as-is; the deps-only
manifest merges the previous closure, so GC never deletes what the
current workspace references. Optional `tong gc --older-than 0` in the
final stage trims unmarked objects to shrink the image — safe there
because the app build's manifest marks the full closure. Never `tong
clean` inside an image — it wipes the store.

## Feature 5: persistent toolchain capture cache `[existing]`

**Problem.** Every `RUN tong build` must establish toolchain identity
before planning. The full sysroot content hash (858 MB, 3,781 files) is
avoided by a per-machine capture cache, but a layer with a cold cache
pays a full capture (~0.28 s), and every warm RUN pays a snapshot check
(~10–30 ms; measured warm capture ~0.03 s) — including the app stage on
every source change (`docs/performance.md`).

**Existing mechanism.** `ToolchainCache` in `tong-rust` stores the rustc
blob + sysroot tree digests + environment bundle (~1–2 MB per
toolchain) in a user-level dir, `$TONG_CACHE_DIR` defaulting to
`~/.cache/tong`, keyed by hash(sysroot canonical path, `rustc -vV`, host
triple). A hit is decided by a cheap per-file `(relpath, mtime_ns,
size, mode)` snapshot over the sysroot; the cached objects are restored
into the project CAS via digest-verified `put_*` calls, so a corrupt
entry degrades to a re-capture, never a wrong identity. The cache
survives `tong clean` (`docs/fingerprint-cache.md`,
`docs/performance.md`).

**`[existing]` pruning (landed 2026-08-09).** Entries (~1–2 MB per
toolchain) are evicted oldest-first — never the entry just written —
when the cache exceeds 64 entries or 512 MB total
(`docs/performance.md` follow-up #1).

**Docker guidance.** The capture cache lives at `~/.cache/tong` inside
the image. With the standard stage flow (Feature 3), the deps stage
performs the cold capture and writes the entry into its layer; the app
stage — `FROM deps` — pays only the snapshot check. No extra wiring is
needed beyond not excluding the cache from the toolchain/deps layers.

One line: the hasher stays SHA-256 — cross-platform golden digest
vectors are a Phase 0 requirement (PLAN.md §4.5, §16), so a blake3
switch remains deferred.

## Cargo-vision features for tong

Curated from
[Ed Page's cargo vision post](https://epage.github.io/blog/2026/08/cargo-vision/).
Tracking-issue numbers verified against the post's "Tracking issue"
lines. Status markers use the convention above; "Proposed tong feature"
is new work this document proposes (or already planned work the item
maps to).

| Vision item (link + tracking issue) | Why it matters for tong | Status | Proposed tong feature | Phase anchor |
|---|---|---|---|---|
| [Docker-friendly caching](https://github.com/rust-lang/cargo/issues/2644) | layer-cacheable builds | `[existing]` | Features 1–3 above (`--deps-only`, fetch/lock pattern, `dockerfile` generator) | Phase 1/2 CLI |
| [Shared caches](https://github.com/rust-lang/cargo/issues/5931) | cross-machine reuse | `[planned: Phase 7]` | remote CAS/action cache (PLAN.md §10.2); plus `tong store export\|import` bundle snapshots — bundle storage already exists (§10) — so CI can seed docker builders | Phase 7 |
| [Build cache GC](https://github.com/rust-lang/cargo/issues/5026) | disk hygiene | `[existing: §10.4]` | `tong gc --older-than 0` guidance for image trimming (Feature 4) | — |
| [Fine-grained cache locking](https://github.com/rust-lang/cargo/issues/4282) | concurrent builds | `[existing: §10.3]` | nothing to add — per-digest atomic writes already, no whole-build locks | — |
| [Removing unnecessary rebuilds](https://github.com/rust-lang/cargo/issues/14604) | smaller rebuild sets | `[planned: §8.3, Phase 5]` | dep-info integration for generated inputs; keep package-level granularity (crate-level compilation granularity is a compiler limitation, PLAN.md §2) | Phase 5 |
| [Test result caching](https://epage.github.io/blog/2026/08/cargo-vision/#test-caching) (no tracking issue) | test-loop speed | `[planned: Phase 8]` | cache test runs keyed by (binary digest, test-data tree, env bundle, execution platform); only deterministic tests | Phase 8 |
| [Iterating on test failures](https://epage.github.io/blog/2026/08/cargo-vision/#test-failures) (no tracking issue) | faster red-green loops | `[planned: Phase 8]` | failure-first reruns | Phase 8 |
| [Structured logging](https://github.com/rust-lang/cargo/issues/15844) | rebuild diagnosis | `[planned: §14]` | persistent `tong log` with historical build events | Phase 1/2 |
| [Plumbing commands](https://epage.github.io/blog/2026/08/cargo-vision/#plumbing-commands) (no tracking issue; project goal linked in the post) | adaptability | `[existing: §14]` | `tong graph --format=json` for CI cache-key scripts | — |
| [Opaque dependencies](https://github.com/rust-lang/cargo/issues/3573) | skip building deps entirely | `[research]` | publish `--deps-only` outputs as opaque bundles; skip dep builds when a matching bundle is present | Phase 7+ |
| [Builtin deps](https://github.com/rust-lang/cargo/issues/16960) + proc-macro/build-script access controls (no tracking issue; compiler-team#1017 referenced) | cache-soundness audit points | `[planned: §8.6]` | keep non-deterministic actions out of shared caches (capture mode never publishes); nothing new | Phase 4/5 |
| [Public dependencies](https://github.com/rust-lang/rust/issues/44663) | doc-only builds | `[not planned]` | defer — needs rustc/cargo ecosystem support tong cannot provide alone | — |

**Exclusions** — upstream/compiler work where tong benefits passively and
none of it is a tong feature: declarative derive macros
([rust#143549](https://github.com/rust-lang/rust/issues/143549)),
alternative compilation models (zig-style lazy builds; the blog's
compilation-model section), the libgit2 migration
([#17227](https://github.com/rust-lang/cargo/issues/17227)), and
async-ifying Cargo
([#16845](https://github.com/rust-lang/cargo/issues/16845)).

## Roadmap and proposed PLAN.md amendments

**Roadmap.**

- **P0 — done (2026-08-09).** Features 1–3: the docker story is
  complete (`--deps-only`, fetch/lock layer pattern, `tong dockerfile`
  generator).
- **P1 — done (2026-08-09).** Feature 5 remaining work: capture-cache
  pruning (64 entries / 512 MB, oldest-first), docker placement
  guidance.
- **P2 — open.** cargo-vision items: `tong store export|import`,
  test-result caching, dep-info integration, opaque bundles.

**Proposed PLAN.md amendments** (applied 2026-08-09 unless noted; PLAN.md
is governing):

- §5: capture-cache semantics — keyed snapshot (toolchain path, version,
  per-file mtime+size), the non-portability marker retained so the
  capture is never published to shared caches. **Applied.**
- §14/§15: add `--deps-only` to `build` and `tong dockerfile` to the
  core command lists. **Applied.**
- Phase 7: add `tong store export|import` bundle snapshots for seeding
  docker builders and CI. **Proposed only** (P2).
- §16: add a docker-stage test — a deps-only build from a
  manifests-only context, then a full build asserting dep actions were
  cache hits. **Applied** (`tong/tests/docker_stage.rs`).

## References

- Ed Page, *A Vision for Cargo*,
  <https://epage.github.io/blog/2026/08/cargo-vision/> (tracking-issue
  links in the table above)
- PLAN.md: §2, §4.1, §4.4, §4.5, §8.3, §8.5, §8.6, §9, §10, §10.1,
  §10.2, §10.3, §10.4, §14, §15 (Phase 1/2, 5, 7, 8), §16
- `docs/performance.md` — measured build costs; toolchain capture cache
  follow-ups
- `docs/fingerprint-cache.md` — capture-cache design and trust model
- `tong/src/main.rs` — current CLI surface (subcommands, flags)
- `tong/src/driver.rs` — `build()` schedule loop, `store_dir`,
  `retention_policy`, `max_size_policy`, `lock`, `fetch`, `gc`
- `tong-rust/src/backend.rs` — `RustBackend::plan()` package source-tree
  capture
- `tong-rust/src/toolchain.rs` — `ToolchainCache`
