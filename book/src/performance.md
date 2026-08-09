# Performance

Measured 2026-08-09 on an Apple M4 Pro (macOS) against Cargo, on the
dual-tool example workspaces (those with `Cargo.toml`). Tong is now at
**1.06–1.15x of `cargo build`** for from-scratch dev builds — down from
**5.7x** when this work started. No-op builds are ~40 ms vs Cargo's
~30 ms.

## Results (median wall seconds, from scratch, warm capture cache)

| Example | Tong | Cargo | Ratio |
|---|---|---|---|
| 01-calc (lib + bin) | 0.30 | 0.26 | 1.15x |
| 02-advanced (build script, cdylib) | 1.25 | 1.14 | 1.10x |
| 05-voxel-city-cargo | 0.80 | 0.73 | 1.10x |
| 06-sdl3-cargo | 0.51 | 0.48 | 1.06x |

## The gap, and how it closed

The entire original gap was **system toolchain capture**: every build
re-hashed the 858 MB rustc sysroot (3,781 files) with software SHA-256
to fingerprint the toolchain closure — 1.21 s per build, warm or cold.

| State | 01-calc tong | cargo | Ratio |
|---|---|---|---|
| original (serial hash) | 1.42 | 0.25 | 5.7x |
| + parallel fingerprint hashing | 0.83 | 0.24 | 3.5x |
| + hardware SHA-256 (sha2 `asm`; identical digests) | 0.61 | 0.24 | 2.5x |
| + capture cache & overlap with model loading | 0.30 | 0.26 | 1.15x |

Tong's compile phase is on par with Cargo's (~0.23 s for the same
crates); the remaining difference is ~30 ms of model loading.

## Where the time goes

Per-phase tracing, forwarded separately from stdout:

```sh
RUST_LOG=tong::perf=debug tong build 2> perf.log
```

Cold-workspace build with a warm capture cache (01-calc, ms):

```text
prepare.open                0    store + action cache open
prepare.model              38    Cargo import + feature resolution
toolchain.cache.snapshot    0    sysroot stat snapshot (parallel)
toolchain.cache.load        2    restore capture into the CAS
prepare.plan                3    plan + topological order
action.execute  calc-core  51    rustc
action.execute  calc-cli   176   rustc
assemble                    1    materialize .tong/out artifacts
record_state                1    build-state manifest + auto-GC
---------------------------------
build.total               277
```

## Why it is fast (and what it costs)

- **Hardware SHA-256** (`sha2` `asm` feature): SHA-NI/ARMv8 SHA-NI
  acceleration, same algorithm and identical digests (golden vectors
  pass).
- **Parallel fingerprint hashing** in `tong-store`: sysroot files are
  hashed concurrently; digests identical to the serial walk.
- **Snapshot-verified capture cache** (`~/.cache/tong`): full content
  capture once per toolchain change; every later build pays a ~10–30 ms
  per-file `(relpath, mtime_ns, size, mode)` stat snapshot, then
  restores the cached capture into the project CAS via digest-verified
  writes. Corrupt entries self-heal to a re-capture — never a wrong
  identity.
- **Capture/model overlap**: the system capture runs on a scoped thread
  while the model loads and features resolve, hiding the capture's
  rustc query under model loading.

This is deliberately *not* Cargo's freshness model. Cargo fingerprints
are `-vV` + profile + dependency fingerprints + source **mtimes**; it
never content-hashes sources and never re-verifies the toolchain
(measurement: `CARGO_LOG=cargo::core::compiler::fingerprint=debug`).
Tong keeps content digests for every action input and for toolchain
identity; only the *cache lookup* for the captured toolchain uses the
stat snapshot, scoped to the rarely-changing sysroot. See
`docs/fingerprint-cache.md` for the full design discussion and trust
model.

## Benchmark harness

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
    done
  done
done
```

Both tools' binaries produce identical output (verified by diff on
01-calc and 02-advanced). Full raw data: `docs/performance.md`.
