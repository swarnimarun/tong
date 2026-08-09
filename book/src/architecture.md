# Architecture

Tong is a Cargo workspace of seven crates with strict dependency
direction:

```text
tong-core  ←  tong-graph / tong-rust  ←  tong-exec      tong-store
        (action schema, digests,     (manifests,      (CAS, caches,
         artifacts, platforms,        labels, plans,    build state,
         providers)                    scheduling)       GC)
                                                          ↓
                                              tong (CLI) on top
```

| Crate | Responsibility |
|---|---|
| `tong` | CLI driver: manifests → toolchain → plan → schedule → execute → assemble; lock/fetch/update, toolchain fetch, dockerfile, gc |
| `tong-core` | Action schema, artifacts, canonical encoding/decoding, digests, platforms, providers |
| `tong-graph` | `Tong.toml` manifest model, labels, planned actions, topological scheduling |
| `tong-rust` | Rust backend: target model, toolchain capture, Cargo import, action planning, build scripts, proc macros, `cc_import` |
| `tong-exec` | Local process executor: deterministic exec roots, clean env, sandbox launchers, output validation |
| `tong-store` | Content-addressed store, tree capture/materialize, bundle storage, action cache, build-state manifests, GC |
| `tong-fetch` | Registry index access and locked source fetching (`Tong.lock`) |

The boundary rules are load-bearing: `tong-core` ←
`tong-graph`/`tong-rust` ← `tong-exec`; `tong-store` is standalone; the
CLI sits on top. No reverse or circular dependencies.

## The pipeline

```text
manifests → resolution → configured target graph
          → backend analysis → immutable action graph
          → execution → content-addressed outputs
```

1. **Manifest loading.** `Tong.toml` (native) or `Cargo.toml` (import).
   In import mode, registry edges are collected and resolved against
   `Tong.lock`; the source store provides package checkouts.
2. **Toolchain.** The Rust toolchain closure is established before
   planning: system capture (rustc + sysroot content-fingerprinted,
   snapshot-cached per machine) or a pinned `dist` bundle. The capture
   runs concurrently with model loading.
3. **Analysis (`tong-rust::RustBackend::plan`).** Each package lowers
   into actions: source materialization, build-script compilation and
   execution, proc-macro compilation for host, library/binary/test
   compilation, linking. Analysis is pure — no ambient file reads, no
   network, no host inspection. Package source trees are captured
   (filtered) into the CAS as action inputs.
4. **Scheduling.** A topological schedule loop concretizes each action,
   computes its digest, looks up the action cache, executes on miss,
   and validates + records the result.
5. **Assembly.** Declared artifacts are materialized under
   `.tong/out/<profile>/`. The build-state manifest is written (the GC
   root) and automatic GC runs.

## The action model

```rust
struct ActionSpec {
    schema_version: u32,

    logical_id: ActionId,          // diagnostics + queries only
    mnemonic: String,

    executable: ArtifactRef,       // artifact, not ambient path
    arguments: Vec<Argument>,
    environment_bundle: Option<BundleRef>,
    environment: BTreeMap<String, String>,

    input_root: TreeDigest,
    declared_outputs: Vec<OutputPath>,
    working_directory: RelativePath,

    execution_platform: PlatformKey,
    target_platform: Option<PlatformKey>,

    timeout: Option<Duration>,
    network_policy: NetworkPolicy,
    cache_policy: CachePolicy,
    resource_requirements: ResourceRequirements,

    properties: BTreeMap<String, CanonicalValue>,  // tong.rust.profile, …
}
```

Semantic digest:

```text
SHA-256(canonical_action_schema_version
        || canonical_semantic_action_fields
        || input_root_digest
        || environment_bundle_digest)
```

Key rules:

- **Logical identity ≠ cache identity.** `logical_id` and mnemonics
  never enter the digest, so target renames, relocations, and
  equivalent graphs reuse cache entries.
- **The executable is an artifact.** Absolute host paths never appear
  in cacheable actions; the executable comes from the input root, a
  declared toolchain closure, or a fingerprinted environment bundle.
- **Unstructured key material is banned.** Extra identity is
  `properties: sorted map<namespace, canonical value>` (e.g.
  `tong.rust.profile`, `tong.rust.rustc_verbose_version`,
  `tong.execution.portable=false`, `tong.execution.network_policy`).
- **Placeholders, not paths.** Exec args use `{exec_root}` /
  `{bundle_root}` substituted by the executor at run time.
- **Output validation is mandatory.** A result is rejected when a
  required output is missing, an undeclared output escapes the output
  tree, an output has the wrong type, the output digest cannot be
  verified, or the executor reports success with an incomplete result.
  Results are committed atomically only after validation.

## Executor

`tong-exec` runs actions with:

- **Deterministic exec roots** — content derived, pruned between
  builds (reproducible from the CAS).
- **Clean environments** — never the parent environment; only
  controlled base values, then the environment bundle, then per-action
  environment (per-action wins).
- **Sandbox launchers** behind one interface: bubblewrap (Linux),
  Seatbelt (macOS, deprecated on Sequoia+), no-op (Windows); opt-in
  via `[policy] sandbox` ([Hermeticity Model](hermeticity.html)).
- **System tools by digest** — the captured rustc is registered as
  `digest → local path` so actions invoke exactly the fingerprinted
  executable.
- **Structured events** — perf/execution tracing via `tracing`
  (target `tong::perf`, `RUST_LOG`-controlled, written to stderr).

Actions carry an `external` ownership flag (workspace vs dependency
package) — this is what `tong build --deps-only` uses to skip local
actions without id parsing.

## Concurrency and state

- Per-digest atomic CAS writes; no whole-build locks; one executor
  claims a missing action, others wait or execute independently.
- Per-project build-state manifests (three newest retained) are the GC
  roots; reachability-based mark-and-sweep enforces retention and size
  budgets ([Caching and the Store](caching.html)).
- All analysis is deterministic: no time, RNG, network, or undeclared
  host state — the configured graph is a pure function of workspace
  inputs, which is what makes shared caches sound.

## Future directions

The action boundary is language-neutral and the internal model maps
losslessly onto REAPI (remote execution) semantics; the next backends
(C/C++), remote caches, and test-result caching are planned — see
[Roadmap](roadmap.html).
