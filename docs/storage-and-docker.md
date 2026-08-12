# Storage, incremental builds, and Docker advantage

Tong should win on storage without losing Cargo's interactive build speed.
That means proving three things together:

1. content is stored once and reused across worktrees;
2. each worktree materializes only what the user asked to consume;
3. no-op and edited builds remain competitive with Cargo.

`PLAN.md` §10.5–10.6 is normative. This document records the current audit and
the implementation order.

## What works now

| Capability | Current evidence |
|---|---|
| Content deduplication | Blobs, trees, action results, sources, and bundles are digest-addressed in `tong-store`. |
| Cross-worktree reuse | Global `--store-dir` or `TONG_STORE_DIR` points multiple workspaces at one store; native and Cargo integration tests prove relocation reuse. |
| Thin project state | In shared mode, the CAS is outside the worktree; `.tong` contains transient exec roots and selected `.tong/out` artifacts. Exec roots are pruned after builds. |
| On-demand outputs | The driver materializes selected top-level artifacts; dependency intermediates stay in the CAS. |
| Correct invalidation | Action keys contain content trees, command/environment/platform/toolchain semantics, and narrowed build-script inputs. `explain rebuild` compares recorded actions. |
| Safe GC | Latest per-project manifests form reachability roots; CAS writes are atomic and immutable; shared-mode clean removes only the caller's roots. |
| Docker layer reuse | `tong build --deps-only`, `tong dockerfile`, and `docker_stage` tests preserve dependency action hits across the app layer. |
| Parallel action scheduling | Dependency-ready build/check/run/test actions use `-j` workers, weighted critical-path priority, digest coalescing, and one inherited GNU-compatible jobserver. |

These are useful foundations, but they do not yet prove a storage or
incremental-speed advantage over Cargo.

## Gaps that block the claim

| Gap | Consequence | Planned proof |
|---|---|---|
| Durable shared backing still requires an environment variable or native-only manifest field | Cargo-import users have a global `--store-dir`, but no persisted workspace-neutral choice. | User config/store setup UX and two-worktree benchmarks. |
| Final artifact materialization copies every blob | Selected outputs duplicate physical bytes in each worktree. | Reflink-first materializer, unchanged-file skipping, byte counters, mutation tests. |
| No physical/logical storage report | Users cannot verify savings or tune GC. | `tong store stats --format text|json` with local/shared/reclaimable accounting. |
| The 24-hour GC floor approximates in-flight safety | Large shared stores retain garbage longer than necessary and lack explicit ownership. | Per-build leases, heartbeat/expiry recovery, concurrent GC tests. |
| Same-workspace builds are serialized | Parallel CLI invocations are safe, but cannot yet share identical work or independently execute divergent subgraphs. | Digest-aware in-flight action claims, wait/reuse semantics, incompatible-flag tests, and removal of the workspace lock. |
| Source trees are content-hashed on every analysis | Very large monorepos can spend too long proving unchanged inputs. | Content-correct stat index with racy-clean checks and miss explanations. |
| Any Rust source edit reruns a whole rustc action without prior incremental state | Edited builds may lose to Cargo even when action lookup is fast. | Bounded local rustc incremental acceleration keyed by canonical unit, with clean-output equivalence checks. |
| Docker support generates files but does not execute actions | Users still assemble and operate the Docker workflow themselves. | `--executor docker`, BuildKit cache transport, capability diagnostics, local/CI parity suite. |

## Implementation sequence

### A. Make sharing obvious and measurable

- Global `--store-dir <path>` works for every command, with precedence over
  `TONG_STORE_DIR`, native `[store] dir`, and the project-local default.
- Add a Cargo/native-neutral user configuration with `store.scope = "user"`
  and an explicit resolved-path diagnostic; never put the path in action
  digests.
- Add `tong store path` and `tong store stats --format text|json`.
- Benchmark two byte-identical worktrees, a renamed worktree, and two unrelated
  projects sharing dependencies. Record both apparent and physical bytes.

### B. Keep worktrees thin

- Record a materialization manifest containing destination, blob digest, mode,
  and requested build selection.
- Skip unchanged destinations and remove stale files owned by the prior
  selection without touching user-created files.
- Try platform copy-on-write cloning (Linux FICLONE, macOS clonefile, Windows
  block cloning where supported), then copy. Never hard-link a writable file to
  the immutable CAS.
- Offer `--materialize none|requested|all`; `requested` remains the default and
  run/test can materialize or execute directly from a transient closure.

### C. Match Cargo on edited builds

- Benchmark and tune the dependency-ready scheduler's historical critical-path
  weights on large corpus workspaces.
- Coordinate overlapping same-workspace builds by complete action digest:
  wait for identical in-flight actions, reuse validated results, and execute
  only differing subgraphs when flags or inputs diverge.
- Add a source fingerprint index as an acceleration layer only. Digest identity
  remains content-based; changed, ambiguous, or racy files are rehashed.
- Consume rustc dep-info for the next action's minimal source tree and explain
  additions/removals.
- Model rustc incremental directories as local, bounded accelerator state.
  They are not uploaded and never change the semantic action key. Periodically
  compare incremental and clean declared outputs; quarantine state on mismatch
  or corruption.
- Run cold, warm, no-op, leaf edit, shared-library edit, feature edit, branch
  switch, and clean rebuild benchmarks against Cargo.

### D. Make Docker an executor

- Introduce a language-neutral executor selection (`local`, `docker`, later
  `remote`) rather than Docker logic in the Rust frontend.
- Discover Docker/BuildKit capabilities through `tong doctor`; pin base images
  by digest in execution identity.
- Use named/cache mounts or OCI cache import/export for CAS transfer. Send a
  minimal generated context derived from declared input trees, not the ambient
  workspace.
- Run fetch actions with declared network access and compile actions with a
  closed network namespace. Preserve UID/GID and selected outputs.
- Differentially execute portable actions locally and in Docker, comparing
  output tree digests, cache hits, cancellation, and structured events.

## Release positioning

- **0.2:** describe the existing shared-store and Dockerfile workflows as
  previews. Do not claim storage superiority until generated measurements are
  checked in and CI-reproducible.
- **0.3:** ship first-class shared-store UX, stats, thin materialization,
  parallel scheduling, and a Docker executor preview. Publish the large-project
  storage and latency report.
- **1.0:** require the PLAN §10.5 competitive thresholds, shared-store lease/GC
  correctness, and equivalent local/Docker portable action results.

## Explicit non-goals

- Tong does not emulate Cargo's `target/` layout or honor
  `CARGO_TARGET_DIR`.
- Shared backing never weakens action identity or permits another workspace to
  mutate cached content.
- Rustc incremental state is a local speed hint, not a portable or remotely
  trusted build result.
- A deployment Dockerfile and a Docker action executor remain separate
  products; one does not silently invoke the other.
