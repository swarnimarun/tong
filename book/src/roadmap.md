# Roadmap

The implementation is driven by `PLAN.md` (the governing design
document) in ten phases. Status reflects what is implemented and
verified in the current tree, not what the plan merely describes.

## Implemented

| Area | Status |
|---|---|
| Phase 0 — specs and invariants | ✅ Canonical action schema, SHA-256 digests with cross-platform golden vectors, hermeticity levels, deterministic encodings, output validation |
| Phase 1 — core pipeline | ✅ Manifest loading, pure resolution, target graph, provider model, action graph, local scheduler, executor, CAS, per-action cache, build-state manifests, reachability GC, `--deps-only` + `dockerfile` CLI |
| Phase 2 — Rust offline MVP | ✅ Native `Tong.toml` targets, Cargo import, workspace/path deps, libs/bins, proc macros, build scripts (with `rerun-if-changed` narrowing), deterministic profiles, Linux/macOS/Windows builds |
| Phase 3 — lockfile and fixed fetching | ✅ `Tong.lock`, registry resolution (cargo-compatible resolver), fixed source fetching with SHA-256 verification, offline enforcement, source store, `tong lock`/`fetch`/`update` — git sources remain open |
| Phase 4 — toolchains and sandboxing | 🟡 System capture + snapshot-verified per-machine cache; pinned `dist` toolchain bundles (`tong toolchain fetch rust`); opt-in sandbox l1–l4 (Linux bubblewrap, macOS Seatbelt, Windows clean-env); macOS ≥ Sequoia documented l1/l2-only |
| Phase 5 — Cargo compatibility | 🟡 Features + unification, build deps, target-specific deps, tests/benches, build-script directive coverage, profiles; differential corpus under `tong-rust/tests/compat/`; rustdoc/doctests, git sources, and remaining edge cases open |
| `tong store export\|import` | 🔜 Proposed (Phase 7 precursor) — seed docker builders and CI with store snapshots |

## Planned

### Phase 6 — C/C++ and native interoperability

- C/C++ toolchain providers, per-translation-unit compile actions
- Header dependency handling (depfiles; conservative header-tree
  declarations until scanning actions exist)
- Static/shared libraries, native tests, Rust↔C/C++ linking
- Runtime closure calculation (ELF/Mach-O/PE) and packaging rules
- CMake/configure/make as compatibility rules, never ambient

### Phase 7 — shared cache and remote execution

- Remote CAS + action cache, auth and namespace policy
- REAPI translation; remote execution client with platform-capability
  matching
- `tong store export|import` bundle snapshots
- Secrets and non-cacheable actions never uploaded; corrupt remote
  blobs detected; local fallback when remote is unavailable

### Phase 8 — test execution platform

- Tests as first-class actions, declared test data, sharding, timeouts,
  flaky-test policy
- Test-result caching (deterministic tests only) and failure-first
  reruns
- Coverage provider interface, structured test reports

### Phase 9 — backend SDK and more languages

- Versioned backend protocol + conformance suite
- Zig/Nim backend, JVM design prototype
- Every future backend must pass the same action-boundary conformance
  tests (no ambient discovery, explicit compiler identity, network-free
  compilation, complete declared outputs, stable action construction)

### Horizontal items

- **Plumbing commands** (`query`, `graph --format=json`, `explain`,
  `log`) for rebuild diagnosis and CI cache keys
- **Dep-info integration** for generated inputs (keeping package-level
  granularity — finer units are a rustc limitation)
- **Opaque dependency bundles** (publish `--deps-only` outputs, skip
  dep builds when a matching bundle exists)
- **Build-script/proc-macro access controls** and audited permission
  declarations; capture mode never publishes to shared caches

## Definition of Tong 1.0

Tong 1.0 does not mean every language. It means:

- Rust, C, and C++ targets coexist in one graph
- Linux, macOS, and Windows have **published certification levels**
- Builds are offline after lock and fetch
- Compilers and SDKs are explicit toolchain closures
- Compile/link actions have clean environments; filesystem and network
  restrictions enforced on certified platforms
- Action digests are stable and cross-machine compatible
- Local CAS, action cache, and reachability-based GC are
  production-ready
- Shared remote caching works
- Build scripts and proc macros are explicit, inspectable actions
- Users can determine exactly why an action rebuilt
- Unsupported compatibility behavior fails clearly
- Every future backend passes the same action-boundary conformance
  suite

## Principal risks

- **Cache unsoundness** — mitigated by conservative inputs first,
  stable hashing before shared caching, mandatory sandbox enforcement,
  adversarial tests, no cross-machine publication from capture mode.
- **Cargo compatibility scope** — published compatibility matrix,
  explicit failure on unsupported semantics, separate import layer,
  common monorepo workflows prioritized.
- **Platform asymmetry** — enforcement levels published per platform;
  no equivalent guarantees claimed until tested; one sandbox interface.
- **Native toolchain discovery** — discovery separated from bundle
  resolution; non-portable bundles clearly marked; cache sharing
  restricted to matching fingerprints.
- **Declarative-model escape pressure** — strictly declared `command`
  rule, first-class rules for repeated patterns, arbitrary logic only
  at the action boundary.
