# Roadmap

`PLAN.md` is the governing design document. This page reports verified
delivery gates, not features that merely have an implementation path.

## Current baseline

The language-neutral action schema, deterministic digests, local CAS, action
cache, build-state manifests, reachability GC, Rust toolchain capture,
`Tong.lock`, registry/git fixed fetching, native `Tong.toml` targets, and a
Cargo-import frontend are implemented.

Cargo-import mode is not yet generally compatible. The pinned required corpus
is the release oracle: resolver and offline-build rows must be green before a
release claims compatibility. Synthetic fixtures remain useful regressions,
but do not replace real-workspace evidence.

## 0.2 — Cargo compatibility preview

Release gates:

- Every required pinned workspace passes resolver comparison and its offline
  Tong build command.
- Resolver 2 and resolver 3 are independently differential-tested; resolver 1
  is rejected with a targeted migration diagnostic.
- Package/action/artifact identities distinguish source, unit kind,
  host/target domain, profile, and features.
- Common host `build`, `check`, `run`, `test`, and `bench` workflows match
  Cargo selection and action behavior.
- Linux L4 sandbox tests pass; macOS and Windows publish the capabilities they
  actually enforce rather than an inferred level.
- All platform tests, package dry-runs, release archive smoke tests, checksums,
  and attestations pass.

## 0.3 — Cargo workflow beta

- Promote rustls, sqlx, bevy, Cargo, and rust-analyzer to the required corpus;
  add larger monorepo and performance fixtures.
- Add Cargo-compatible `metadata` and `tree`, common selection/configuration
  flags, parallel action scheduling, and certified cross-target Rust builds.
- Support build-affecting Cargo configuration with explicit handling for
  credentials and home configuration.
- Publish a field-by-field Cargo manifest, command, and platform matrix.

The Cargo workflow scope is build, check, run, test, bench, doc, resolution,
fixed fetching, metadata, and dependency-tree inspection. Cargo publishing,
installation, project generation, vendoring, and dependency editing are not
Tong goals.

## 1.0 — Generalized hermetic builds

- Rust, C, and C++ targets coexist in one language-neutral action graph.
- Linux, macOS, and Windows have published sandbox capability certification.
- Builds are offline after lock/fetch and toolchains are explicit closures.
- Local and shared caches pass corruption, concurrency, platform, and secret
  isolation tests.
- Build scripts and proc macros are explicit, inspectable actions.
- Rebuilds are explainable and every backend passes the same conformance suite.

## Later capabilities

- C/C++ toolchains, translation-unit actions, native linking, runtime closure,
  and audited CMake/configure/make compatibility actions.
- Remote CAS/action cache and REAPI execution with capability matching.
- Test sharding, deterministic result caching, coverage, and structured reports.
- A versioned backend SDK for additional languages.

## Principal risks

- **Cache unsoundness:** conservative inputs, stable hashing, adversarial tests,
  and no shared publication from unverified capture modes.
- **Cargo semantic drift:** pinned Cargo comparisons, separate resolver 2/3
  certification, source-qualified identities, and explicit unsupported errors.
- **Platform asymmetry:** capability reporting instead of equivalent-level
  claims where operating systems cannot enforce the same isolation.
- **Large-workspace performance:** dependency-ready parallel scheduling,
  bounded resources, persistent capture caches, and measured corpus budgets.
