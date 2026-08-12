# Tong vs Cargo: from-scratch build performance

## 2026-08-12 large-workspace cached-build milestone

The release-built Tong binary was measured on the pinned `tracing` corpus
after warming both the Tong cache and filesystem cache. The graph contains 454
actions and every measured action was a validated cache hit.

| Metric | Before | After |
|---|---:|---:|
| Tong internal wall time | 10.12 s | 0.90 s median |
| Preparing | 3.20 s | ~0.80 s |
| Checking cache | 6.29 s | ~0.10 s |
| Finishing/state/GC | 0.28 s | ~0 s on unchanged state |

After enabling the checksum-sidecar fast path, five release-binary wall-clock
samples were 1.66, 0.90, 0.90, 0.91, and 0.89 seconds. The first sample
followed a release link and was retained in the raw list; the warmed median is
0.90 seconds. Tong's emitted total for the warm samples had a median of 0.896
seconds. This establishes the first sub-second cached-build milestone on the
fixture.

The improvement came from build-scoped state/result/closure memoization,
persistent input-tree assembly, a content-correct source snapshot index,
decoded-tree caching, skipping duplicate state and automatic GC on no-op
builds, and avoiding per-hit progress lines by default. The remaining dominant
phase is Cargo manifest/model import plus source metadata validation during
`Preparing`; action concretization and cache validation are no longer the
bottleneck.

`tools/cache-stability.sh` is the release-only correctness harness. It rejects
debug Tong binaries and checks exact cache decisions for no-op, mtime-only,
unused/unrelated file, preserved-mtime leaf edit, and edit-revert scenarios on
the checked-in `examples/09-cache-stability` DAG.

Measured 2026-08-09 on `examples/` workspaces that both tools can build.
Purpose: quantify the clean-build gap, find where tong's time goes, and
track regressions.

## Summary

- From-scratch `tong build` is now **1.06–1.15x of `cargo build`** (dev
  profile) on the dual-tool examples — down from **5.7x** at the start of
  this work.
- The entire original gap was **system toolchain capture**: every build
  re-hashed the 858 MB rustc sysroot (3,781 files) with software SHA-256
  to fingerprint the toolchain closure (PLAN.md §5) — 1.21 s per build,
  warm or cold.
- Fixes, in order:
  1. Parallel fingerprint hashing (`tong-store`): capture 1.21 s → 0.64 s.
  2. Hardware SHA-256 (sha2 `asm` feature, identical digests): 0.64 s →
     0.28 s.
  3. Per-machine toolchain capture cache (`~/.cache/tong`, survives
     `tong clean`, stat-snapshot invalidation) + capture overlapped with
     model loading: capture ~0.03 s, and a fully-cached no-op build is
     ~40 ms — cargo parity.
- Tong's compile phase is on par with cargo's (~0.23 s for the same
  crates). Remaining difference: ~30 ms of model (Cargo-import) loading.
- Design rationale (hermetic execution + snapshot-verified capture cache,
  what we deliberately do not copy from cargo): `docs/fingerprint-cache.md`.

## Scope

Only examples both tools can build are compared. Tong imports plain Cargo
workspaces, so the Cargo-manifest examples are dual-tool; the `Tong.toml`
examples have no `Cargo.toml` and cannot be built by cargo.

| Example | Actions | Description |
|---|---|---|
| `01-calc` | 2 | `calc-core` lib + `calc-cli` bin |
| `02-advanced` | 6 | build script, `advanced-core` (cdylib+lib), `shout`, `advanced-app` |
| `05-voxel-city-cargo` | 4 | `voxel-city` bin + external `sdl3-sys` from `06-sdl3-cargo` |
| `06-sdl3-cargo` | 3 | `sdl3-sys` lib with build script (native SDL3 link) |

Excluded: `01-hello`, `03-sdl3`, `04-voxel-city` (no `Cargo.toml`).

## Environment

- macOS 26.3.1, Apple M4 Pro, 14 cores, 48 GB RAM
- rustc 1.97.1, cargo 1.97.1, tong 0.1.0 (release build
  `target/release/tong`, rebuilt from the measured change `vtzqqpnp`)
- SDL3 present at `/opt/homebrew/opt/sdl3` (needed by `05`, `06`)
- dev profile on both sides (`tong build`, `cargo build`), no network
  (all examples are path-only dependencies)

## Methodology

From-scratch builds only, per the benchmark intent:

1. Before every timed run, remove **both** tools' artifacts outside the
   timed window: `tong clean` (removes `.tong/`, including the project
   store) and `rm -rf target` (cargo's artifacts).
2. Time the build with `/usr/bin/time -p`, wall clock.
3. 5 rounds per project, alternating tool order each round to spread
   thermal/frequency drift.
4. Both commands succeed on every run (exit 0); binaries produced by the
   two tools produce identical output (`01-calc`, `02-advanced` verified
   by diff).

Note: since the capture cache landed, `tong clean` no longer resets the
toolchain capture — it lives in `$TONG_CACHE_DIR` (default
`~/.cache/tong`). The first build after a toolchain change pays a full
capture (~0.28 s); all subsequent builds hit the cache. The benchmark
numbers below are warm-cache (round 1 included; it is indistinguishable
from rounds 2–5 because the cache survived the earlier runs).

Harness (runs from the repo root):

```bash
#!/bin/bash
ROOT=$PWD; TONG=$ROOT/target/release/tong; OUT=/tmp/tong-bench
PROJECTS="01-calc 02-advanced 05-voxel-city-cargo 06-sdl3-cargo"
for p in $PROJECTS; do
  for r in $(seq 1 5); do
    [ $((r % 2)) -eq 1 ] && TOOLS="tong cargo" || TOOLS="cargo tong"
    for t in $TOOLS; do
      (cd $ROOT/examples/$p && $TONG clean >/dev/null 2>&1; rm -rf target)
      CMD=$([ $t = tong ] && echo $TONG || echo cargo)
      /usr/bin/time -p bash -c "cd $ROOT/examples/$p && $CMD build > $OUT/${p}_${t}_r$r.log 2>&1" \
        2> $OUT/${p}_${t}_r$r.time
      echo "${p}_${t}_r$r exit=$? real=$(grep '^real' $OUT/${p}_${t}_r$r.time | awk '{print $2}')s"
    done
  done
done
```

## Results

Median wall time, seconds (5 runs, from scratch; capture cache warm).

| Example | tong | cargo | ratio |
|---|---|---|---|
| 01-calc | 0.30 | 0.26 | 1.15x |
| 02-advanced | 1.25 | 1.14 | 1.10x |
| 05-voxel-city-cargo | 0.80 | 0.73 | 1.10x |
| 06-sdl3-cargo | 0.51 | 0.48 | 1.06x |

The improvement path on `01-calc` (same machine, same code):

| state | tong | cargo | ratio |
|---|---|---|---|
| original (serial hash) | 1.42 | 0.25 | 5.7x |
| + parallel fingerprint hashing | 0.83 | 0.24 | 3.5x |
| + hardware SHA-256 | 0.61 | 0.24 | 2.5x |
| + capture cache & overlap | 0.30 | 0.26 | 1.15x |

## Where the time goes

Collect per-phase metrics for lock, fetch, and build with the built-in tracing
(stderr, so it can be forwarded separately):

```sh
RUST_LOG=tong::perf=debug tong lock 2> lock-perf.log
RUST_LOG=tong::perf=debug tong fetch 2> fetch-perf.log
RUST_LOG=tong::perf=debug tong build 2> build-perf.log
```

Human output also prints a duration for each command phase. With
`--message-format json`, phase and command completion events include
`duration_ms`; fetch additionally emits one timed `fetch-finished` event per
source. Locked registry and git sources are fetched concurrently, bounded by
the lesser of the host's available parallelism and eight workers.

`01-calc`, cold-workspace build with a warm capture cache (ms):

```
prepare.open                0    store + action cache open
prepare.model              38    Cargo import + feature resolution
toolchain.cache.snapshot    0    sysroot stat snapshot (parallel)
toolchain.cache.load        2    restore capture into the CAS
prepare.plan                3    plan + topological order
action.execute  calc-core  51    rustc
action.execute  calc-cli   176    rustc
assemble                    1    materialize .tong/out artifacts
record_state                1    build-state manifest + auto-GC
---------------------------------
build.total               277
```

Compile time (227 ms) ≈ cargo's ~0.19 s. Tong's remaining overhead is
~40 ms of model loading and ~3 ms of cache mechanics. A no-op build
(everything cached) is ~40 ms total — cargo's no-op is ~30 ms.

## What cargo does (confirmed, for comparison)

`CARGO_LOG=cargo::core::compiler::fingerprint=debug cargo build` on
`01-calc` shows cargo's freshness model is **mtime-based, not
content-based**:

- per-unit fingerprint = rustc `-vV` string + profile hash + dependency
  fingerprints + source **mtimes** from rustc's dep-info; a missing
  fingerprint file marks the unit dirty
- no content hashing of sources and **no toolchain re-verification** —
  a sysroot change with the same `-vV` is invisible to cargo
- clean build: ~5 ms unit graph + ~5 ms fingerprint check + ~190 ms rustc
  + ~10 ms dep-info/fingerprint/artifact finalize; no-op build: ~30 ms

Tong's stricter identity (content digests, PLAN.md §3.2/§4/§8.4) is what
made the sysroot hash expensive; the capture cache restores cargo-level
speed without dropping the content identity.

## Fixes

1. **Parallel fingerprint hashing** — `tong-store` `fingerprint_dir`
   hashes files concurrently (`collect_files` + `hash_files_parallel` +
   `build_fingerprint_tree`); digests identical to the serial walk.
2. **Hardware SHA-256** — `sha2` `asm` feature (sha2-asm, ARMv8/x86
   SHA-NI). Same algorithm, identical digests (golden vectors pass);
   ~4x per-core throughput.
3. **Per-machine toolchain capture cache** — `tong-rust`
   `ToolchainCache`: user-level dir (`$TONG_CACHE_DIR`, default
   `~/.cache/tong`), keyed by the canonical sysroot path, invalidated by
   a per-file `(relpath, mtime_ns, size, mode)` snapshot (parallel stat,
   ~0–3 ms). On a hit the capture (rustc blob, sysroot trees, bundle) is
   restored into the project CAS via digest-verified `put_*` calls; a
   corrupt or partial entry degrades to a re-capture, never a wrong
   identity. The sysroot is fully content-hashed on first capture and on
   any snapshot change.
4. **Capture/model overlap** — the system capture runs on a scoped
   thread while `prepare()` loads the model and resolves features; the
   capture's rustc query is hidden under model loading.

## Follow-ups

- ~~Cache GC~~ — done 2026-08-09: entries are evicted oldest-first
  (never the just-written entry) beyond 64 entries / 512 MB total
  (`ToolchainCache::prune`).
- Phase 3 (git-style stat fast path for source trees) and Phase 4
  (shared/remote cache) remain deferred — see
  `docs/fingerprint-cache.md`.
- Warm-build and incremental-build timings are out of scope here; the
  tracing events above make them easy to add later.

## Raw data (seconds, wall)

Final state, run order as executed:

| run | 01-calc tong | 01-calc cargo | 02-adv tong | 02-adv cargo | 05 tong | 05 cargo | 06 tong | 06 cargo |
|---|---|---|---|---|---|---|---|---|
| 1 | 0.29 | 0.26 | 1.86 | 1.43 | 1.10 | 1.04 | 0.82 | 0.78 |
| 2 | 0.31 | 0.26 | 1.25 | 1.14 | 0.77 | 0.73 | 0.54 | 0.49 |
| 3 | 0.30 | 0.25 | 1.25 | 1.18 | 0.81 | 0.72 | 0.51 | 0.47 |
| 4 | 0.29 | 0.26 | 1.25 | 1.12 | 0.80 | 0.73 | 0.51 | 0.47 |
| 5 | 0.30 | 0.26 | 1.22 | 1.15 | 0.81 | 0.73 | 0.51 | 0.48 |

Round 1 is consistently elevated for both tools (page-cache warmup after
the harness start); medians are reported above.
