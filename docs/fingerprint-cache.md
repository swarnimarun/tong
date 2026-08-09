# Toolchain freshness: Cargo-style mtime checks or hermetic content hashing?

Status: **implemented 2026-08-09 (Phase 1 + Phase 2; Phases 3–4 deferred).**
This document records the design; the code is in `tong-rust`
(`ToolchainCache`) and `tong-store` (parallel fingerprint hashing).
Measured outcome: `01-calc` clean build 1.42 s → 0.30 s (5.7x → 1.15x of
cargo); no-op build ~40 ms. Full numbers in `docs/performance.md`.

## The question

From-scratch builds are now near-parity on compile time (tong compiles the
same crates in ~0.23 s vs cargo's ~0.19 s), and the measured gap is a
constant ~0.6 s of **system toolchain capture** (re-hashing the 858 MB
rustc sysroot every build, warm or cold). The obvious fix — stop
re-hashing when nothing changed — raises a design question: adopt cargo's
mtime-based freshness model, or stay with full content hashing?

Short answer: **neither extreme. Keep hermetic execution and content
digests; add a snapshot-verified capture cache.** Cargo-style freshness
for the toolchain *cache lookup* is fine; cargo-style *identity* (version
string only) is not.

## What cargo actually verifies (confirmed by measurement, 2026-08-09)

`CARGO_LOG=cargo::core::compiler::fingerprint=debug` on `examples/01-calc`:

| cargo step | cost |
|---|---|
| manifest load + unit graph (features, profile, targets) | ~5 ms |
| per-unit fingerprint check: read `.fingerprint/<hash>/<target>`; missing → dirty | ~5 ms |
| `rustc` × 2 (the compile itself) | ~190 ms |
| dep-info write, fingerprint write, artifact finalize | ~10 ms |
| **no-op build total** | **~30 ms** |

Cargo's fingerprint is a hash of: rustc `-vV` string, profile settings,
feature resolution, dependency fingerprints, and **source file mtimes**
(taken from rustc's dep-info). Cargo never content-hashes sources, never
re-verifies the toolchain, and its freshness rule is mtime arithmetic
("max output mtime ≥ max dep mtime" → fresh).

Failure modes cargo accepts:

- source edited with preserved mtime/size (clock skew, `touch -r`,
  coarse-granularity filesystems) → **stale outputs, silently**
- toolchain internally changed with the same `-vV` (rustup reinstall of
  the same version, corrupted sysroot, updated libstd without version
  bump) → **undetected, artifacts change under the same key**

Cargo gets away with this because it is fast and its ecosystem tolerates
the residual risk.

## What tong verifies (current)

- Every action input and the toolchain are **content-addressed**
  (PLAN.md §3.2 pure analysis, §4 action digests). A changed byte always
  changes the digest; nothing is ever "fresh by mtime".
- Toolchain identity includes **sysroot content** (PLAN.md §8.4), not
  just `-vV`.
- Cost: the sysroot is fully re-hashed every build, warm or cold
  (~0.64 s after the parallel-hashing fix; 858 MB, 3,781 files, sha2
  software SHA-256).

## Hermeticity vs freshness — they are orthogonal

- **Hermetic execution**: what an action may *observe* — declared inputs
  only, clean env, sandbox, no network. This is tong's product identity;
  keep it unconditionally.
- **Freshness**: how we decide "input X is unchanged since the cached
  digest was computed" — byte-hash (strong) or stat data (cheap). This is
  an implementation detail of the *cache layer*; it does not change what
  actions can observe, and it does not change what the digest *means*.

The design question is only about the freshness signal for cached
digests — not about execution semantics.

## Options

### A. Cargo-style everywhere (mtime freshness for sources and toolchain)

Cheapest (~30 ms no-op), proven at scale, but:

- loses content-addressed identity (PLAN §3.2/§4 redesign),
- inherits cargo's stale-output failure modes,
- toolchain identity becomes `-vV` only — sysroot corruption or a
  same-version stdlib change silently produces new outputs under old
  cache keys.

**Not recommended.** It is the one option that changes what tong *is*.

### B. Status quo: full content hashing every build

Strongest detection, simple, already parallelized. Pays ~0.64 s per
build forever, including no-op builds; cost multiplies per captured
toolchain (cc, other languages) and hurts future shared-cache work.

### C. Hybrid (recommended): hermetic execution + snapshot-verified capture cache

- Action digests keep covering **toolchain content** (PLAN §8.4): the
  capture cache stores the *result* of a full content hash, and the
  digest value is unchanged — we only stop *recomputing* it.
- "Toolchain unchanged" is decided by a per-file snapshot:
  `(relpath, mtime_ns, size, mode)` over sysroot `bin/` + `lib/`,
  folded into one digest with the existing Hasher (~10–30 ms to
  recompute — 3,781 stats).
- Snapshot differs → full re-capture (content hash, new digests,
  actions invalidate). Every real toolchain change (rustup, brew,
  manual install) bumps mtimes; only an *adversarial* mtime+size
  preservation would be missed — the same trust model as git's stat
  cache, rustc incremental, and cargo dep-info, but scoped to the
  rarely-changing toolchain instead of user sources.
- Cache lives **outside the project** (user-level dir), so `tong clean`
  stops destroying the capture; clean and warm builds both skip it.
- No-op builds go from ~0.56 s to ~40 ms — cargo parity, with stronger
  correctness than cargo.

Safety properties:

- false-dirty (touch without change) is safe — costs one re-capture
- false-fresh requires deliberate stat forgery — out of scope, same as
  every incremental tool
- cached objects are digest-named and re-verified on restore
  (`cas.put_*` recomputes); corrupt cache self-heals to a re-capture

## Phased plan

### Phase 1 — hardware SHA-256 (zero-risk, ~4x)

sha2 0.10 already ships `asm` (aarch64/x86 SHA-NI) — same SHA-256
algorithm, **identical digests**, just faster. Enable the feature in
`tong-core`; golden digest tests + NIST vectors verify identity.
Capture: ~0.64 s → ~0.15 s. One-line dependency change; no design risk.

### Phase 2 — persistent capture cache (the main fix)

Components:

1. `tong-rust/toolchain.rs`: `ToolchainCache`
   - key = hash(sysroot canonical path, `rustc -vV`, host triple)
   - snapshot digest = canonical fold of sorted per-file
     `(relpath, mtime_ns, size, mode)` for sysroot `bin/` + `lib/`
   - objects cached: rustc blob bytes, bin/lib/sysroot tree encodings,
     environment bundle encoding (≈1–2 MB total)
2. Lookup path: recompute snapshot (~10–30 ms) → compare with cached →
   hit: restore objects into the project CAS via existing
   `put_blob`/`put_tree`/`put_bundle` (each self-verifying) → miss:
   full capture as today, then store.
3. Cache location: `$TONG_CACHE_DIR` → `$HOME/.cache/tong` (unix);
   atomic writes (tmp + rename, marker file commits the entry); corrupt
   or partial entries are treated as misses and overwritten.
4. No GC initially (entries are ~1–2 MB per toolchain); note in docs.
5. PLAN.md §5 alignment note: the cache is per-machine local, never
   published to a shared cache; toolchain identity remains a content
   digest in every action.

Tests (fake-sysroot harness with `TONG_RUSTC` wrapper):

- capture twice → identical digests, second capture skips hashing
- modify a sysroot file → snapshot changes → re-capture, digests change
- `touch` a sysroot file (mtime bump, same content) → re-capture
  (false-dirty direction is safe)
- corrupt a cached object → self-heal to re-capture
- `tong clean` → capture survives; next build is a cache hit

Targets (measured baseline: cargo clean 0.24 s / no-op 0.03 s;
tong clean 0.83 s / no-op 0.56 s on 01-calc):

| metric | phase 1 | phase 2 |
|---|---|---|
| clean build (01-calc) | ~0.35 s | ~0.26 s |
| no-op build | ~0.25 s | ~0.04 s |

### Phase 3 — deferred: source-tree stat fast path (git-style)

Only if source hashing ever dominates (very large monorepos): stat
snapshot first, re-hash on stat change (racy-clean handling like git's
index). Separate correctness review; not needed for the current examples
(source hashing is single-digit ms).

### Phase 4 — deferred: shared/remote cache

A per-machine capture cache is the natural seed for it: capture once,
reuse everywhere the toolchain is identical.

## What we deliberately do not copy from cargo

- `-vV`-only toolchain identity (misses sysroot content changes)
- mtime-only source freshness without content verification
- ambient reads / unverified build-script behavior

## Decision needed

All approved 2026-08-09:

1. ✅ Phase 1 (sha2 `asm`) — implemented.
2. ✅ Phase 2 design — user-level cache location
   (`$TONG_CACHE_DIR` → `~/.cache/tong`) and the mtime+size+mode snapshot
   trust model for the toolchain only — implemented.
3. ✅ Phases 3/4 deferred.

Full measurement methodology and raw data: `docs/performance.md`.
