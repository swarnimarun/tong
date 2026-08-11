# Caching and the Store

Every byte Tong produces or consumes lives in one content-addressed
store (`CAS`): blobs, trees, actions, action results, sources,
toolchains, environment bundles, and build-state manifests. Identity
comes exclusively from digests; human-readable names are only
diagnostics.

## Digests

- **Algorithm:** SHA-256 with canonical encodings (canonical map
  ordering, string/path encoding, symlink representation, executable
  bits, mode normalization). Cross-platform golden vectors are a Phase
  0 requirement — digests are identical on every platform.
- **Scope:** an action digest covers the canonical semantic fields, the
  input-root tree digest, the environment-bundle digest, and the
  toolchain-closure digests. It never includes `logical_id`, target
  names, mnemonics, or absolute host paths — so renames, relocations,
  and equivalent graphs all share cache entries. Exec roots appear only
  as `{exec_root}`/`{bundle_root}` placeholders substituted by the
  executor.
- **Freshness:** digests are content-based. The one deliberate
  exception is the *toolchain capture cache* (below), which trades
  stat-snapshot freshness for speed while keeping the digest semantics
  identical.

## Store layout

```text
<store>/
  blobs/       files
  trees/       directory trees (Merkle)
  actions/     action specs
  results/     action results
  sources/     locked package sources
  toolchains/  toolchain closures
  bundles/     environment bundles
  state/       build-state manifests
  tmp/         transient writes
```

Default location: `<root>/.tong/store`. Relocate (shared-store mode)
with global `--store-dir`, `TONG_STORE_DIR`, or `[store] dir` in
`Tong.toml` (native mode only). `tong store path` prints the effective
location; `--format json` emits a versioned machine-readable result.

Materialized files are writable copies today; they are not hard links to
immutable CAS blobs. Reflink-first thin materialization and physical-byte
accounting are tracked in `PLAN.md` §10.5.

**Atomicity and concurrency.** CAS writes are temp-file → digest
verify → atomic rename → immutable final object, so concurrent writers
are tolerated. There are no whole-build locks: multiple readers are
always allowed, one executor claims a missing action, others wait or
execute independently, and failed or partial results are never
committed as successful cache records.

## The action cache

The per-action cache is keyed by the action digest. On a cache hit the
executor materializes the recorded result (outputs, exit status) from
the CAS without running anything. Because analysis is pure and the
digest covers the complete input set, a hit is always sound — Tong
never caches under a key computed before the complete input set was
known (build scripts narrow their inputs via `rerun-if-changed`
directives after their first run).

A no-change build therefore executes zero actions:

```text
build complete: 3 actions (3 cached, 0 executed)
```

## Build-state manifests and GC

After each build, a per-project build-state manifest records the full
object closure of that build: top-level targets and the configured
graph digest, every action (digest, logical id, mnemonic, inputs,
outputs, stdout/stderr, duration), the union of source input roots and
toolchain bundles, and the materialized artifact names.

- The three newest manifests per project are retained; a rebuild makes
  the previous graph's objects garbage immediately.
- Only the *latest* manifest of each project is a GC root. GC is
  reachability-based mark-and-sweep: mark everything reachable from the
  latest manifests (trees keep their blobs and subtrees, bundles keep
  their files trees), then delete unmarked `results/` entries and
  objects older than the retention floor (default `7d`), oldest-first
  when the store exceeds the size budget (default `10G`). Objects
  younger than 24h are never deleted (protects concurrent in-flight
  builds in shared mode). Stale `tmp/` files older than 24h are always
  deleted.
- Automatic GC runs after every build (best-effort; failures only
  warn). Manual sweep: `tong gc [--older-than <dur>] [--max-size <size>]
  [--dry-run]` (`--older-than 0` deletes all unmarked immediately).
  Policy comes from `[store] retention`/`[store] max_size` or
  `TONG_STORE_RETENTION`/`TONG_STORE_MAX_SIZE`.

## Shared stores

Point several workspaces (or CI machines) at one store with
`TONG_STORE_DIR` or `[store] dir`. All writes stay atomic and
idempotent per digest; no locks are needed. In shared mode:

- `tong clean` removes only the calling project's exec roots, outputs,
  and state manifests, then sweeps the objects no other project's
  manifest marks — never another project's live objects.
- `--deps-only` builds merge the previous manifest's closure into their
  own, so a narrow dep-only manifest never orphans the local cache.

## Toolchain capture cache

System toolchain capture content-hashes the rustc sysroot (hundreds of
MB). To avoid re-hashing on every build, the capture *result* is cached
per machine:

- Location: `$TONG_CACHE_DIR` (default `~/.cache/tong`), outside the
  project — survives `tong clean`.
- Key: hash of the sysroot canonical path, `rustc -vV`, and host
  triple. Freshness: a cheap per-file `(relpath, mtime_ns, size, mode)`
  snapshot over the sysroot (~10–30 ms). A changed snapshot triggers a
  full content re-capture; a hit restores the cached objects into the
  project CAS through digest-verified `put_*` calls, so a corrupt entry
  degrades to a re-capture, never a wrong identity.
- The cache is per-machine: captures remain non-portable and are never
  published to shared caches. Entries are pruned oldest-first (never
  the just-written entry) beyond 64 entries / 512 MB.

This is the same trust model as git's stat cache and rustc incremental,
but scoped to the rarely-changing toolchain: false-dirty (a touch)
costs one re-capture; false-fresh requires deliberate stat forgery.

## Environment bundles

Toolchain identity includes the environment the compiler needs:
controlled variables plus the artifacts they reference (SDK files,
runtime libraries, linkers). A bundle digest represents the referenced
closure, not the text of the variables. System captures are marked
non-portable; `dist` toolchains are portable bundles resolved from the
store.
