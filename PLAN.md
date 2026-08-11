# Tong Hermetic Build System

## Reimplementation and Delivery Plan

## 1. Executive Summary

Tong should be implemented as a **language-neutral build engine with declarative language frontends**, rather than as a Rust package manager that later accumulates other languages.

The fundamental pipeline is:

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

Every backend—Rust, C, C++, Zig, JVM, or another ecosystem—must lower targets into the same immutable action representation. The scheduler, cache, store, sandbox, query system, remote execution support, and diagnostics must have no Rust-specific behavior.

Tong should initially certify:

* Rust builds on Linux, macOS, and Windows.
* Native Rust interoperability with C and C++.
* Offline builds from a lockfile.
* Explicit toolchains and dependency artifacts.
* Enforced filesystem and network restrictions on certified platforms.
* Local content-addressed caching.
* Reproducible action descriptions and cross-machine cache compatibility where execution platforms and toolchain closures match.

“All languages and all platforms” should be an extensibility property, not an initial compatibility claim. Tong should publish a capability matrix showing which languages, toolchains, sandbox guarantees, and execution modes are certified on each platform.

Cargo’s current architectural limitations around plumbing interfaces, structured diagnostics, shared caches, cache locking, build-script control, and test caching directly support Tong’s action-oriented design.

---

## 2. Product Definition

Tong is:

> A declarative, hermetic, multi-language build system for monorepos that preserves familiar language workflows while lowering all build work into explicit, cacheable actions.

Tong is not initially:

* A universal package registry.
* A replacement compiler.
* A programming language for arbitrary build logic.
* A replacement for Cargo's publishing, installation, project-generation,
  vendoring, or dependency-editing commands.
* A Nix distribution or Nix expression evaluator.
* A guarantee that artifacts produced for different target platforms are identical.
* A solution to Rust compiler limitations such as crate-level compilation granularity or the absence of a stable Rust ABI.

Tong can exploit compiler capabilities such as Rust metadata artifacts and dep-info, but it cannot independently introduce finer Rust compilation units. Similarly, Tong can model prebuilt or opaque Rust artifacts, but general opaque Rust dependencies require compiler and ABI support beyond the build system.

For Rust, Tong does aim to replace Cargo's stable **build workflow**:
manifest and configuration loading, resolver versions 2 and 3, locking and
fixed fetching, build, check, run, test, bench, doc, metadata, and dependency
tree inspection. Cargo-import compatibility belongs to the Rust frontend; it
must not add Rust-specific behavior to the action schema, scheduler, store,
sandbox, or remote-execution interfaces.

---

## 3. Governing Design Principles

### 3.1 Actions are the only execution unit

Backends may resolve packages, inspect manifests, and construct graphs, but all compilation, linking, code generation, testing, packaging, and tool invocation must become actions.

A backend must never invoke Cargo, CMake, a compiler driver, or another package manager as an ambient subprocess during normal analysis.

Compatibility adapters may run legacy build systems as explicitly declared actions, with declared inputs, outputs, toolchains, environment, and sandbox policy.

### 3.2 Analysis is pure

Given:

* Workspace manifests.
* Lockfile.
* Platform definitions.
* Selected configuration.
* Toolchain metadata.
* Dependency provider data.

Analysis must produce the same configured graph and action graph.

Analysis must not:

* Read arbitrary files that were not part of the workspace input.
* Access the network.
* Inspect undeclared host environment variables.
* Run compiler discovery commands.
* Depend on current time, random numbers, home-directory configuration, or mutable global state.

### 3.3 Toolchains are dependencies

A compiler is not a host property. It is an input closure containing:

* Executables.
* Runtime libraries.
* Standard libraries or sysroots.
* Linkers and archivers.
* SDK files.
* Configuration metadata.
* Required environment values.
* Host, execution, and target constraints.
* A stable fingerprint.

Bazel similarly distinguishes host, execution, and target platforms and resolves toolchains against platform constraints. Tong should retain these distinctions while offering a smaller declarative surface.

### 3.4 Hermeticity and reproducibility are separate claims

A hermetic action only observes declared inputs and capabilities.

A reproducible action produces equivalent outputs when executed again with the same inputs.

Tong should report these independently:

```text
Hermeticity: enforced
Reproducibility: verified
Cacheability: enabled
Remote eligibility: enabled
```

Some hermetic actions may still be non-reproducible because the compiler embeds timestamps, paths, random values, or unstable ordering.

Tong exposes two execution modes over the same action graph and CAS:

* `compat` is the migration/performance mode. It aims to run ordinary Cargo
  projects like Cargo, including build scripts and proc macros that have not
  been audited yet. It may permit compatibility accesses, but reports
  `Hermeticity: not enforced`; actions whose undeclared behavior can affect
  outputs are local-only or uncacheable and are never uploaded to shared or
  remote action caches.
* `hermetic` is the security mode. It grants only declared capabilities from
  the permission file and enforces the selected platform sandbox. Missing
  permissions fail; they are never inferred during a normal build.

Cargo import defaults to `compat` so adoption does not require rewriting every
build script first. A workspace may opt into `hermetic` once permissions are
reviewed. Both modes retain content-addressed source, toolchain, and declared
input storage, so compatibility mode still benefits from shared worktrees and
small `.tong` directories without making a security claim.

### 3.5 No embedded build language in the initial product

Normal users should describe targets as data in TOML.

Tong should not initially introduce a Starlark-like programming language. This is one of the main opportunities to remain simpler than Bazel.

Extensibility should be provided through:

1. Built-in declarative rule schemas.
2. Versioned backend implementations.
3. A future process or WASM-based backend protocol.
4. Pure templates and configuration expansion.

Native dynamic plugins should be avoided because Rust does not provide a stable language ABI.

---

## 4. Required Corrections to the Current Action Model

The existing action boundary is directionally correct, but several fields need stricter semantics.

### 4.1 Separate logical identity from cache identity

An action should have:

* `logical_id`: graph identity used for diagnostics and queries.
* `digest`: semantic execution identity used for caching.

`logical_id` should not normally be included in the action digest. Including a target name or graph-local ID prevents cache reuse after harmless target renames, workspace relocation, or equivalent graph construction.

Similarly, `mnemonic` should be diagnostic metadata unless it changes execution semantics.

### 4.2 The executable must be an artifact, not an ambient path

Replace:

```text
executable path
```

with:

```text
executable: ArtifactRef
```

Absolute host paths must not appear in cacheable actions. The executable must either:

* Exist in the action input root.
* Be supplied by a declared toolchain closure.
* Be a platform-provided artifact represented by a fingerprinted environment bundle.

### 4.3 Replace unstructured extra key material

`extra key material` is too unconstrained and can become a cache-correctness escape hatch.

Use:

```text
properties: sorted map<namespace, canonical value>
```

Examples:

```text
tong.rust.profile
tong.rust.rustc_verbose_version
tong.cc.abi
tong.execution.network_policy
```

All property values must use a canonical encoding.

### 4.4 Canonical action schema

A suitable conceptual model is:

```rust
struct ActionSpec {
    schema_version: u32,

    logical_id: ActionId,
    mnemonic: String,

    executable: ArtifactRef,
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

    properties: BTreeMap<String, CanonicalValue>,
}
```

The semantic digest should be:

```text
SHA-256(
    canonical_action_schema_version
    || canonical_semantic_action_fields
    || input_root_digest
    || environment_bundle_digest
)
```

The input root is a Merkle tree containing source files, generated artifacts, compiler binaries, relevant SDK content, and dependency outputs.

The Remote Execution API also models execution through an action referring to a command and content-addressed input directory. Tong’s internal representation should map cleanly to this model without making REAPI its internal storage format.

### 4.5 Stable hashing is a Phase 0 requirement

Stable cryptographic hashing cannot remain a later roadmap item.

Before shared caching, Tong must define:

* SHA-256 as the initial mandatory digest algorithm.
* Canonical map ordering.
* Canonical string and path encoding.
* Symlink representation.
* Executable-bit handling.
* File mode normalization.
* Path separator normalization.
* Case-sensitivity rules.
* Schema-version behavior.
* Unknown-field behavior.
* Digest test vectors shared by all platforms.

Without these rules, cross-machine cache compatibility is impossible.

### 4.6 Mandatory output validation

A successful action result must be rejected when:

* A required output is missing.
* An undeclared output escapes the output tree.
* An output path has an unexpected type.
* The output digest cannot be verified.
* The executor reports success but returns an incomplete result.

Action results must be committed atomically only after all required outputs have been validated.

---

## 5. Environment Bundle Model

An environment bundle must represent both variables and the artifacts those variables reference.

```rust
struct EnvironmentBundle {
    name: String,
    provider: String,
    platform: PlatformKey,

    variables: BTreeMap<String, String>,
    files: TreeDigest,
    metadata: BTreeMap<String, CanonicalValue>,

    digest: Digest,
}
```

For example, a Windows MSVC bundle may include:

* `PATH`
* `INCLUDE`
* `LIB`
* `LIBPATH`
* MSVC compiler and linker binaries.
* Windows SDK tools.
* SDK headers and import libraries.
* Universal CRT files.
* Toolchain version and discovery metadata.

The bundle digest must represent the referenced closure, not merely the text of the environment variables.

Merge semantics should be:

```text
base deterministic action environment
        ↓
environment bundle
        ↓
per-action generated environment
```

The per-action environment wins on conflicts.

### Deterministic base environment

Tong must not inherit the parent process environment by default.

The base should provide only controlled values such as:

* Sandbox-specific `TMPDIR`, `TMP`, and `TEMP`.
* A sandbox-specific `HOME` or equivalent.
* A configured locale.
* A deterministic working directory.
* Explicit path-remapping variables where required.

`PATH`, compiler flags, SDK variables, package-manager configuration, proxy variables, credentials, and user configuration must never be inherited implicitly.

Secret values require a separate facility. An action receiving secrets should be non-cacheable and ineligible for general remote execution unless a secure executor policy explicitly supports it.

### Linux and Nix

Nix-backed bundles are valuable because a Nix derivation describes a build step with defined inputs, outputs, process fields, and a system type, while Nix closures represent reachable build or runtime dependencies.

However, Nix should be one environment-bundle provider, not a mandatory Linux dependency. Tong must also support:

* Downloaded content-addressed toolchains.
* Container or SDK-derived toolchains.
* System toolchain capture for local-only compatibility.
* Organization-provided toolchain bundles.

A system-captured bundle should be marked non-portable unless all referenced files have been imported and fingerprinted.

### System toolchain capture cache

Per-machine toolchain captures (docs/fingerprint-cache.md) are cached
outside the project (`$TONG_CACHE_DIR`, default `~/.cache/tong`) and keyed
by a cheap stat snapshot — toolchain path, `rustc -vV`, and per-file
`(relpath, mtime_ns, size, mode)` over the sysroot — so warm builds pay a
~10–30 ms snapshot instead of re-hashing the sysroot. A changed snapshot
triggers a full content re-capture; the cache never invents digests, and
cached objects are digest-verified on restore. The cache is per-machine:
captures remain non-portable and are never published to shared caches.
Entries are pruned oldest-first beyond 64 entries / 512 MB.

---

## 6. Target, Rule, and Provider Model

### 6.1 Canonical workspace manifest

Use `Tong.toml` as the canonical build description.

Cargo manifests should be accepted through an import adapter, but Cargo compatibility must not define the core data model.

Example:

```toml
[workspace]
members = [
    "crates/*",
    "native/*",
]

[policy]
network = "fetch-only"
undeclared_reads = "error"
undeclared_writes = "error"

[toolchain.rust]
version = "1.90.0"
targets = ["x86_64-unknown-linux-gnu"]

[toolchain.cc]
kind = "clang"
version = "21"

[target.native_codec]
rule = "cc_library"
srcs = ["native/codec/*.cc"]
hdrs = ["native/codec/include/**/*.h"]
public_includes = ["native/codec/include"]

[target.codec]
rule = "rust_library"
crate_root = "crates/codec/src/lib.rs"
deps = [":native_codec"]

[target.application]
rule = "rust_binary"
crate_root = "crates/application/src/main.rs"
deps = [":codec"]
```

### 6.2 Initial built-in rules

The first supported rule set should be deliberately small:

```text
filegroup
alias
command
fetch
rust_library
rust_binary
rust_proc_macro
rust_test
rust_build_script
cc_library
cc_binary
cc_test
archive
package
```

The `command` rule must:

* Use an argument array rather than an implicit shell.
* Declare all tools.
* Declare all inputs and outputs.
* Run without network by default.
* Require an explicit shell toolchain when shell interpretation is needed.

### 6.3 Providers

Targets should exchange typed provider data rather than exposing backend internals.

Core providers:

```text
DefaultInfo
FilesInfo
RunfilesInfo
TestInfo
PackageInfo
RuntimeClosureInfo
```

Language providers:

```text
RustCrateInfo
RustProcMacroInfo
CcInfo
LinkInfo
GeneratedSourceInfo
```

A backend analysis operation conceptually becomes:

```rust
fn analyze(
    target: ConfiguredTarget,
    dependencies: ProviderMap,
    context: AnalysisContext,
) -> Result<AnalysisResult>;
```

`AnalysisResult` contains:

* Actions.
* Providers.
* Diagnostics.
* Optional policy declarations.

---

## 7. Platform and Configuration Model

Tong must explicitly distinguish:

* **Host platform:** where the Tong client runs.
* **Execution platform:** where an action executes.
* **Target platform:** where the resulting artifact runs.

A platform is a canonical set of constraints:

```toml
[platform.linux_x86_64]
os = "linux"
arch = "x86_64"
abi = "gnu"

[platform.windows_x86_64_msvc]
os = "windows"
arch = "x86_64"
abi = "msvc"
```

Additional constraints may include:

* Compiler ABI.
* C runtime.
* Minimum OS version.
* CPU features.
* SDK version.
* Accelerator availability.
* Container or executor capabilities.

Toolchain resolution must be deterministic and explainable:

```text
tong query toolchain //crates/app:application
tong explain toolchain //crates/app:application
```

The explanation must show:

* Required toolchain types.
* Candidate toolchains.
* Rejected constraints.
* Selected execution platform.
* Selected target platform.

Configuration transitions should remain restricted. Tong should initially support explicit host and target configurations rather than a general transition language that can cause combinatorial graph expansion.

---

## 8. Rust Backend Plan

### 8.1 Compatibility strategy

Rust support should have two modes:

1. **Native Tong mode:** targets are defined directly in `Tong.toml`.
2. **Cargo import mode:** `Cargo.toml` is translated into Tong targets;
   `Tong.lock` remains Tong's exact, source-qualified lock and fixed-source
   record.

The imported graph must remain inspectable without invoking Cargo:

```text
tong query targets
tong query deps //crates/app
tong query actions //crates/app
tong graph --format=json
tong metadata --format-version=1
tong tree
```

Cargo should not be invoked during builds.

### 8.2 Rust action types

A Rust package may lower into:

```text
source materialization
build-dependency compilation
build-script compilation
build-script execution
proc-macro compilation for host
Rust metadata/check compilation
Rust library compilation
binary or test compilation
linking
test execution
documentation
packaging
```

Host and target artifacts must remain distinct. Proc macros and build scripts execute on the execution platform even during cross-compilation.

### 8.3 Source inputs

The safe initial policy is to include the entire package source tree, excluding known output directories, as the compile action input.

This is coarse but correct.

A later optimization may use:

* Rust dep-info.
* Compiler-discovered module files.
* Generated-source manifests.
* A dedicated dependency-discovery action.

Discovered inputs must affect the final compile action digest. Tong must not cache a compile result under a key created before the complete input set is known.

### 8.4 Rust toolchain identity

The Rust toolchain closure should include:

* Resolved `rustc`.
* `rustdoc`.
* Standard libraries for every required target.
* Linker configuration.
* LLVM tools when requested.
* Sysroot content.
* `rustc -vV` output.
* Toolchain manifest and installation source.
* Required native runtime artifacts.

The rustup shim must never be the executable in a cacheable action.

### 8.5 Compiler determinism

Cacheable Rust actions should initially disable or isolate compiler incremental state.

Tong should apply path remapping where supported so workspace and sandbox paths do not leak into outputs.

The profile must explicitly encode:

* Optimization level.
* Debug information.
* Panic strategy.
* LTO.
* Codegen units.
* Overflow checks.
* Assertions.
* Strip policy.
* Linker behavior.
* Target CPU and features.
* Unstable compiler options.

### 8.6 Build scripts

Build scripts require an explicit compatibility ladder.

Build scripts and proc macros are untrusted user programs. They run as
ordinary action processes inside the same capability broker and sandbox as
other actions. Compiling a proc macro is not permission to let the resulting
binary escape the consumer's sandbox. The broker records attempted reads,
writes, environment lookups, child-process launches, and network connections
with the action identity and phase (`build-script`, `proc-macro`, or `rustc`).

The same permission model applies while a proc macro executes during rustc
expansion; it is not treated as trusted compiler code.

#### Strict declarative mode

Common operations should be represented directly:

* Native source compilation.
* Generated constants.
* Version metadata.
* Link search paths.
* Linked libraries.
* Generated bindings from a declared tool.
* File copying.
* Environment exports.

#### Audited build-script mode

A build script declares requested capabilities:

```toml
[build_script.permissions]
read = ["schema/**", "vendor/**"]
write = ["$OUT_DIR/**"]
env = ["TARGET", "HOST"]
network = false
process = ["//tools:protoc"]
```

Proc macros use the same shape, with permissions scoped to the source-qualified
package and host unit:

```toml
[permissions.package_name.proc_macro]
read = ["templates/**"]
write = []
env = ["CARGO_PKG_VERSION"]
process = ["//tools:helper"]
network = false
```

For Cargo-import workspaces these declarations live in a checked-in
`Tong.permissions.toml` companion file, not in `Cargo.toml`, so Cargo can
continue to parse the project unchanged. Native workspaces may inline the
same tables. `$OUT_DIR` is the only implicit writable location; every other
write requires a declaration.

#### Compatibility capture mode

During migration, Tong may run a build script in a tracing sandbox, report observed accesses, and generate a candidate permission declaration.

Capture mode must not be considered fully hermetic and must not publish shared-cache results by default. `tong audit permissions` (or
`tong build --audit-permissions`) records successful and denied filesystem,
environment, process, and network attempts and emits a minimal candidate TOML
plus a human-readable diff. Tracing is best-effort and reports what could not
be observed.

Hermetic-mode builds are enforcement-only: they load the checked-in permission file,
grant exactly those capabilities, and fail with a targeted remediation when a
new access is attempted. An interactive build may show the capability diff and
ask the user to approve writing the companion file; it never silently broadens
the current action. Non-interactive builds and CI never prompt and print the
exact TOML entry needed instead. Network-enabled permissions make the action
uncacheable and are marked in build events. Permission changes are action
inputs and invalidate affected actions.

Compatibility mode remains available for projects that intentionally prioritize
Cargo behavior over isolation. Its build-script and proc-macro actions carry a
visible compatibility marker, default to local-only caching, and cannot make a
hermetic or remote-cache claim. Users can migrate one package at a time by
auditing it and moving its permissions into the hermetic mode file.

Build-script and proc-macro access controls are particularly important because they execute as a prerequisite to compilation and can undermine deterministic caching. Reducing common build-script behavior to declarative rules should be a first-class Rust-backend objective.

### 8.7 Feature resolution

Implement Cargo-compatible feature unification as a pure resolver component with:

* Independently tested resolver-version 2 and 3 semantics.
* Host/build/target dependency separation.
* Optional dependencies.
* Default-feature control.
* Target-specific dependencies.
* Workspace inheritance.
* Weak dependency features.
* Dependency feature propagation.

The resolved feature graph must be serializable and independently testable.

Use differential tests against Cargo for representative workspaces, but treat the Tong resolver output as a versioned internal format.

Resolver 1 is deliberately unsupported for now. An explicit resolver 1, or
an older-edition workspace whose Cargo default is resolver 1, must fail with
a targeted diagnostic suggesting an explicit workspace `resolver = "2"`.
Tong must never silently apply resolver 2 to a resolver-1 workspace.

---

## 9. Source Resolution and Lockfiles

`Tong.lock` should exist before crates.io resolution or networked builds become generally available.

Each locked dependency must record:

```text
package identity
version
source kind
source location identity
source revision
content checksum
manifest checksum
subdirectory
patch or override identity
provenance metadata
```

Supported source kinds:

```text
workspace
local path
git commit
fixed archive
crate archive
registry package
generated source
```

Only fixed-output fetch actions may access the network.

A fetch action must know its expected digest before executing:

```text
URL or source identity
expected SHA-256
output object type
optional signature or provenance policy
```

Commands should be separated:

```text
tong lock
tong fetch
tong build --offline
tong update <package>
```

A normal build must not silently mutate the lockfile or access the network.

---

## 10. Content-Addressed Store and Cache

Use one logical content-addressed storage layer with separate namespaces:

```text
store/
  blobs/
  trees/
  actions/
  results/
  sources/
  toolchains/
  bundles/
  state/
```

Human-readable names may be appended for diagnostics, but identity comes exclusively from digests.

### 10.1 Atomic operation

CAS writes must use:

1. Temporary file or directory.
2. Digest verification.
3. Atomic rename.
4. Immutable final object.
5. Duplicate-writer tolerance.

### 10.2 Cache layers

Tong should support:

```text
in-process memoization
local action cache
local CAS
shared remote action cache
shared remote CAS
remote execution
```

Remote execution must come after remote caching. The internal action representation should map losslessly to REAPI so organizations can reuse existing cache and executor infrastructure.

### 10.3 Concurrent builds

The current correctness baseline serializes mutating build commands in the
same workspace with a project lock. This prevents a second CLI invocation from
pruning execution roots or rewriting project state while the first build still
uses them. It is intentionally an interim safety measure, not the target
concurrency model.

The planned scheduler must replace whole-workspace serialization with
digest-aware in-flight coordination:

* Multiple readers and builds in different workspaces are always allowed.
* A complete action digest identifies compatible in-flight work.
* One executor may claim a missing action while compatible clients wait for
  and reuse its validated result.
* Builds whose flags, inputs, toolchains, or platforms produce different
  digests execute only their divergent subgraphs independently.
* Dependency-ready actions within one build run in parallel under explicit
  `-j` resource accounting.
* Completed CAS objects are immutable; failed and partial results are never
  committed as successful cache records.

Removing the workspace lock requires adversarial multi-process tests covering
identical builds, partially overlapping graphs, incompatible flags, failures,
interruptions, action-cache publication, output materialization, state writes,
and concurrent garbage collection.

### 10.4 Retention and garbage collection

After a successful build, write a build-state manifest (`tong-store::state`,
`<store>/state/projects/<project_hash>/`) recording the full object closure
of the build:

* Requested top-level targets and the configured target graph digest
  (canonical sorted `(logical_id, action_digest)` pairs).
* Every action: digest, logical id, mnemonic, input root, executable blob,
  environment bundle, outputs, stdout/stderr, duration.
* The union of source input roots and toolchain bundles.
* Materialized artifact name → outputs tree.

The three newest manifests per project are retained on disk (superseded
state is deleted at write time — "rebuilding clears the old cache"); only
the *latest* manifest of each project is a GC root, so a rebuild makes the
previous graph's objects garbage immediately. A `--deps-only` build
records only dependency actions and invalidates nothing: its manifest
merges the previous manifest's object closure (actions, sources,
toolchains, artifacts — deduped by digest) plus every captured package
tree, so GC roots always cover what the current workspace references and
never orphan the local cache (docs/docker-caching.md).

Garbage collection (`tong-store::gc`) is reachability-based mark-and-sweep
over the whole store, mirroring Cargo's GC direction (#5026/#16804) but
using CAS reachability instead of SQLite mtime tracking:

```text
roots:      latest successful graph per project
            (every digest in every manifest from StateStore::all())
            + transitively: tree → blobs/subtrees, bundle → files tree
sweep:      unmarked results/ entries and objects older than the
            retention floor (default 7d) are deleted, results first
budget:     when the store exceeds max_size (default 10G), unmarked
            objects are deleted oldest-first until under budget, never
            younger than 24h (protects concurrent in-flight builds in
            shared mode)
tmp:        stale <store>/tmp files older than 24h are always deleted
```

The mark phase closes the reachability graph (a marked tree keeps its
blobs and subtrees; a marked bundle keeps its files tree), so GC never
sweeps content a marked object references. Retention and budget are
configured via `[store] retention`/`[store] max_size` in `Tong.toml` or
the `TONG_STORE_RETENTION`/`TONG_STORE_MAX_SIZE` environment variables;
`tong gc [--older-than <dur>] [--max-size <size>] [--dry-run]` runs a
manual sweep (defaults from config; `--older-than 0` deletes all unmarked
immediately).

Shared stores (env `TONG_STORE_DIR` or `[store] dir`) are first-class: all
writes stay atomic and idempotent per digest, no locks are needed
(§10.1, §10.3), and `tong clean` in shared mode removes only the calling
project's exec roots, outputs, and state manifests, then sweeps the
objects that no other project's manifest marks — it never touches another
project's live objects.

The Nix concept of retaining complete reachable closures is useful here, but Tong should maintain separate build-time and runtime reachability.

### 10.5 Storage and incremental-build advantage

Storage efficiency is a product contract, not an incidental CAS property.
Tong must keep each worktree's `.tong` directory thin while one optional
user-level store owns content shared by worktrees and unrelated workspaces.
The detailed current-state audit and implementation sequence live in
`docs/storage-and-docker.md`.

The required model is:

```text
worktree/.tong/          shared store (one per user or explicit path)
  out/ selected files     blobs/ content stored once
  exec/ transient         trees/ metadata stored once
                         results/ action results stored once
                         state/projects/ GC roots per workspace
```

Required behavior:

* `TONG_STORE_DIR` remains supported, but a global `--store-dir` option and a
  durable, workspace-neutral user configuration must make shared backing an
  obvious one-command choice in both Cargo-import and native mode.
* Only requested final artifacts are materialized under `.tong/out`; dependency
  outputs, metadata, and intermediate codegen remain in the shared CAS. Check,
  metadata, and graph-only commands materialize no user artifacts.
* Materialization must prefer safe copy-on-write clones (reflinks) and fall
  back to copies. It must never hard-link writable output to an immutable CAS
  blob. Repeated materialization must skip files whose digest and mode already
  match, and stale artifacts from the same selection must be removed.
* Identical source, toolchain, and action outputs occupy physical storage once
  across worktrees. State manifests and selected output directory entries are
  the only unavoidable per-worktree overhead.
* GC must use explicit leases for running builds and atomic root updates;
  elapsed age alone is not a concurrency guarantee. Size/age policies operate
  on physical bytes and never evict a leased or reachable closure.
* `tong store stats --format text|json` must report project-local bytes,
  physical shared bytes, logical referenced bytes, deduplication ratio,
  materialized bytes, reclaimable bytes, and roots by project. `tong clean`
  must report exactly which project-local data and roots it removed.
* Source invalidation remains content-correct. A snapshot index may avoid
  rehashing unchanged source files, but changed or racy entries are hashed and
  every action key continues to contain content digests. Rust dep-info and
  build-script rerun declarations narrow subsequent input trees.
* Rustc incremental state is an optional local acceleration input, never part
  of cross-machine cache identity. It must be keyed by canonical Rust unit,
  toolchain, and profile, captured outside final artifacts, bounded by GC, and
  periodically checked against a clean build. A missing or corrupt state must
  only cost time, never change correctness.

Competitive gates use a pinned large-workspace benchmark on the same machine
and warmed filesystem cache:

* no-op wall time is no slower than Cargo by more than 10% or 20 ms, whichever
  is larger;
* a representative one-file edit is no slower than Cargo by more than 10%;
* a second identical worktree with shared backing adds at most 2% of the first
  build's physical store bytes plus its selected final artifacts;
* `.tong` in shared mode contains no dependency intermediates and is smaller
  than Cargo's project-local `target` directory for every required benchmark;
* cache diagnostics explain every miss as an input, command, environment,
  toolchain, platform, policy, or prior-state change.

Benchmark reports must include cold, warm, no-op, one-file edit, branch switch,
second-worktree, GC, and Docker/CI scenarios. Performance claims in release
documentation must link to a generated report with machine, filesystem,
toolchain, corpus revision, physical-byte accounting method, and raw samples.

### 10.6 Docker-native execution and cache transport

`tong dockerfile` and `tong build --deps-only` are the existing layer-cache
path; they are not yet a Docker executor. Native Docker support must add an
executor selected with `--executor docker` (and the same executor interface
used by future remote execution), while preserving the action graph and cache
keys used by local execution.

The Docker executor must:

* use BuildKit when available and fail with an actionable capability report;
* mount or import/export the Tong CAS without copying it into every layer;
* key builder caches by execution platform and toolchain/image digest, never a
  mutable image tag alone;
* deny action network access by default, allowing it only for explicit fetch
  actions, and report the capabilities actually achieved;
* preserve host UID/GID ownership, executable modes, symlinks, cancellation,
  structured events, and selected-artifact materialization;
* support local Docker, rootless Docker, CI cache export/import, multi-platform
  builds, and an offline mode whose network namespace is demonstrably closed;
* avoid sending `.git`, `.tong`, Cargo `target`, secrets, or unrelated
  workspace files in the build context.

The generated-Dockerfile workflow remains supported for deployment images.
The native executor is for running Tong actions in containers; these are
separate interfaces and must have separate tests and documentation.

---

## 11. Sandboxing and Enforcement

`env_clear` alone is not hermeticity. It only reduces environmental inputs.

Sandbox selection is subordinate to the execution mode:

```toml
[policy]
mode = "compat"   # default for Cargo import; broad Cargo behavior
sandbox = "l1"    # compatibility baseline

# Security-focused workspaces use:
# mode = "hermetic"
# sandbox = "auto" | "l3" | "l4"
```

`compat` may use a clean environment or a platform-specific compatibility
wrapper, but reports that undeclared host access is possible. It is the
general-purpose build path and its unsafe actions are not shared-cache
eligible. `hermetic` selects the strongest requested/available enforcement,
loads `Tong.permissions.toml`, and refuses to proceed when the platform cannot
enforce the declared policy. The action schema remains language-neutral; mode,
capabilities, and achieved enforcement are execution inputs and events.

Tong must define enforcement levels:

```text
Level 0: declared action, no enforcement
Level 1: clean environment
Level 2: isolated writable output and temporary directories
Level 3: undeclared filesystem reads denied
Level 4: network denied
Level 5: restricted process and system capabilities
```

Only Levels 3 and 4 should qualify as “hermetic” for general cache publication.

### Linux

Target implementation:

* User and mount namespaces.
* Read-only input mounts.
* Writable output and temporary mounts.
* Network namespace.
* Optional seccomp profile.
* Process-tree termination.
* Resource limits.

### macOS

Target implementation:

* Dedicated sandbox launcher.
* Read-only staged input tree.
* Restricted writable directories.
* Network denial where reliably enforceable.
* Process-tree monitoring.
* Explicitly documented limitations for each macOS release.

### Windows

Target implementation:

* Job Objects.
* Restricted tokens.
* ACL-isolated execution roots.
* Explicit writable output directories.
* Process-tree termination.
* Network restriction using supported Windows isolation facilities.
* File access tracing for compatibility diagnosis.

Sandboxing may remain opt-in during development, but it must be a release gate before a platform is advertised as hermetic.

---

## 12. C and C++ as the Second Backend

The supplied roadmap proposes Zig or Nim before JVM because their compilation models resemble Rust. That is technically convenient but does not validate Tong’s stated Rust-plus-native-monorepo objective.

C and C++ should be the second backend.

It forces the core architecture to solve:

* Header dependency discovery.
* Per-translation-unit actions.
* Static and shared libraries.
* ABI-sensitive toolchain configuration.
* Native linking.
* Runtime library closure.
* Cross-language provider exchange.
* SDK and system-library isolation.
* Platform-specific link behavior.

Initial native rules:

```text
cc_library
cc_binary
cc_test
cc_import
cc_tool
```

Each translation unit should compile separately.

Header discovery can initially use compiler-generated depfiles. For strict caching, Tong should either:

* Conservatively declare the complete header tree, or
* Run a dependency-scanning action before compilation.

A compile action cannot be shared safely until its complete transitive header set is represented in the input root.

### Rust and C/C++ interoperability

`CcInfo` should expose:

```text
public headers
include roots
defines
static libraries
shared libraries
link arguments
runtime files
ABI metadata
```

A Rust target consuming `CcInfo` lowers it into explicit `rustc` linker arguments and runtime dependencies.

A Rust library exported to C should be able to produce:

```text
staticlib
cdylib
generated C header
runtime closure
package metadata
```

### Legacy native build systems

CMake, configure, make, Meson, and similar tools should be compatibility rules, not privileged execution paths.

Example:

```toml
[target.legacy_library]
rule = "cmake_project"
srcs = ["vendor/library/**"]
toolchain = "cc"
outputs = ["lib/liblegacy.a", "include/**"]
network = false
```

These rules run inside normal actions and export typed providers.

---

## 13. Runtime Closure and Packaging

Runtime linking cannot remain a distant phase if mixed Rust and C++ applications are a core use case.

Runtime closure calculation should be implemented with the C/C++ backend:

### Linux

* Determine ELF shared-library dependencies.
* Control or patch `RUNPATH`.
* Avoid ambient `/usr/lib` discovery.
* Include the selected dynamic loader where packaging requires it.

### macOS

* Resolve Mach-O dependencies.
* Control install names.
* Set or patch `LC_RPATH`.
* Include permitted framework and dylib dependencies.

### Windows

* Resolve DLL dependencies.
* Distinguish system DLLs from application-distributed DLLs.
* Copy runtime DLL closure or generate a controlled launcher.
* Record MSVC runtime requirements.

Packaging rules should consume `RuntimeClosureInfo`, not independently rescan arbitrary host state.

---

## 14. Observability and Developer Experience

Tong should expose both porcelain commands and plumbing data.

Core commands:

```text
tong build --deps-only
tong dockerfile
tong build
tong check
tong test
tong run
tong fetch
tong lock
tong update
tong query
tong graph
tong explain
tong clean
tong doctor
```

`tong build --deps-only` executes only actions owned by non-workspace
packages (docs/docker-caching.md): docker dep layers bust only when the
lockfile or toolchain changes. `tong dockerfile` generates the
layer-cache-friendly `Dockerfile` + `.dockerignore` for the workspace.

Required diagnostic workflows:

```text
tong explain rebuild //app
tong query actions //app
tong query inputs <action>
tong query outputs <action>
tong query critical-path
tong graph --format=json
tong log --build <id>
```

Every execution should emit structured events containing:

* Action logical ID and digest.
* Queue, execution, and materialization timing.
* Cache lookup source.
* Cache hit or miss.
* Reason for miss.
* Executor identity.
* Input and output digests.
* Exit status.
* Sanitized command.
* Sandbox violations.
* Peak memory and CPU where available.

Cargo’s vision specifically identifies plumbing commands and structured historical logging as mechanisms for adaptability and rebuild diagnosis.

---

## 15. Revised Implementation Phases

The numbered phases below describe architectural dependencies, not current
completion. Delivery is controlled by the following evidence-based milestones.
Documentation must derive status from the same compatibility reports used by
CI; implementation alone is not a completed compatibility claim.

### 0.2 — Cargo compatibility preview

Release only when:

* The required pinned Cargo corpus passes resolver and offline-build gates.
* Resolver 2 and resolver 3 synthetic graphs match Cargo; resolver 1 fails
  explicitly.
* Package, action, and artifact identities distinguish name, version, source,
  target, profile, feature domain, and compile mode.
* Common host build/check/run/test/bench workflows work without Cargo.
* Linux L4 sandbox adversarial tests pass and every platform reports achieved
  enforcement without silent downgrade claims.
* Full validation, package dry-runs, binary smoke tests, checksums, and release
  attestations pass.

### 0.3 — Cargo workflow beta

* Promote the extended corpus to required and add large monorepo/performance
  fixtures.
* Add Cargo-compatible metadata/tree interfaces, common selection flags,
  configuration precedence, parallel scheduling, and certified cross-target
  builds.
* Publish a field-by-field manifest/configuration compatibility matrix.
* Make shared backing a first-class CLI/config choice, publish physical-storage
  and incremental-build comparisons with Cargo, and add safe thin-output
  materialization.
* Add the Docker executor preview with BuildKit cache persistence and honest
  sandbox-capability reporting.

### 1.0 — Generalized hermetic build system

* Retain the Cargo workflow gates while Rust, C, and C++ coexist in one action
  graph.
* Require published sandbox capability certification and production shared
  cache correctness.
* Require every backend to pass the language-neutral action-boundary
  conformance suite.
* Meet the storage, second-worktree, no-op, and one-file-edit competitive gates
  in §10.5 on the published large-workspace tier.
* Certify the local and Docker executors against the same action, cache,
  cancellation, offline, and adversarial-sandbox suites.

### Architectural phase ordering

## Phase 0 — Specifications and Invariants

### Deliverables

* Versioned action schema.
* Canonical digest algorithm.
* Path and tree encoding rules.
* Platform schema.
* Toolchain schema.
* Environment-bundle schema.
* Action-result schema.
* Build-state manifest schema.
* Error and diagnostic format.
* Cross-platform digest test vectors.
* Hermeticity-level definitions.

### Exit criteria

* Linux, macOS, and Windows compute identical digests for the same synthetic action.
* Action serialization is deterministic.
* Logical target renames do not alter action digests unless execution semantics change.
* Missing outputs and malformed cache results are rejected.

---

## Phase 1 — Core Pipeline Reimplementation

### Deliverables

* Manifest loading.
* Pure configuration resolution.
* Configured target graph.
* Provider model.
* Action graph.
* Local scheduler.
* Local process executor.
* Immutable local CAS.
* Per-action cache.
* Structured event log.
* `query`, `graph`, and `explain` plumbing.
* `build --deps-only` and `dockerfile` CLI generation.

### Repository strategy

Keep the existing five public crates while moving implementation into private modules. Split crates only after an actual dependency or ownership boundary is demonstrated.

Suggested internal modules:

```text
tong-core/
  action
  artifact
  digest
  platform
  provider
  diagnostics

tong-graph/
  labels
  configured_targets
  analysis
  scheduling

tong-exec/
  local
  process
  results
  events

tong-store/
  cas
  action_cache
  state
  gc
```

### Exit criteria

* A synthetic multi-target graph executes incrementally.
* Concurrent builds do not lock the whole output tree. *(partial: CAS writes
  are per-digest and execution roots are isolated, but same-workspace builds
  remain serialized until §10.3 in-flight coordination lands.)*
* Cache hits survive process restarts. *(completed: digest-keyed results with build-state manifests and reachability GC — §10.4)*
* `tong explain rebuild` reports the changed semantic input.

---

## Phase 2 — Rust Offline MVP

### Deliverables

* Native Rust targets.
* Cargo manifest import.
* Workspace and path dependencies.
* Rust libraries and binaries.
* Host proc macros.
* Simple build scripts.
* Explicit `--extern` artifacts.
* Deterministic Rust profiles.
* Linux, macOS, and Windows smoke builds.

### Exit criteria

* A representative multi-crate workspace builds without invoking Cargo.
* The build runs with an empty inherited environment.
* Rebuilding without changes executes no compile or link actions.
* A private implementation-only source change rebuilds only actions whose declared inputs changed.
* Proc macros and build scripts appear as explicit actions.

---

## Phase 3 — Lockfile, Fixed Fetching, and Source Store

### Deliverables

* `Tong.lock`.
* Git fixed revisions.
* Fixed archive sources.
* Crate archives.
* Registry resolution.
* SHA-256 verification.
* Offline build enforcement.
* Source provenance metadata.
* Content-addressed source storage.

### Exit criteria

* `tong build --offline` succeeds from a populated store.
* A checksum mismatch fails before analysis consumes the source.
* A normal build cannot access the network.
* Equivalent dependencies are deduplicated in the source store.

---

## Phase 4 — Platform Toolchains and Sandbox Enforcement

### Deliverables

* Downloaded Rust toolchain bundles.
* Nix bundle provider.
* macOS Xcode/SDK provider.
* Windows MSVC/SDK provider.
* Linux sandbox.
* macOS sandbox.
* Windows sandbox.
* Undeclared-read and undeclared-write diagnostics.
* Network denial.

### Exit criteria

* Certified Rust builds cannot read the user home directory.
* Build scripts cannot write outside declared outputs.
* Network attempts fail during compile and build-script actions.
* Toolchain replacement changes the action digest.
* Cache results are shared only between compatible execution-platform fingerprints.

---

## Phase 5 — Cargo Compatibility

### Deliverables

* Cargo-compatible feature resolution.
* Build dependencies.
* Target-specific dependencies.
* Test and example targets.
* Additional crate types.
* Complete supported build-script directives.
* Rerun directives.
* Generated inputs.
* Dep-info integration.
* Rustdoc and documentation tests.
* Differential compatibility suite.

### Exit criteria

* A published compatibility corpus has documented pass, unsupported, and divergent cases.
* Unsupported Cargo behavior fails with a targeted diagnostic rather than silently changing semantics.
* Feature-resolution results match Cargo for the supported corpus.

---

## Phase 6 — C/C++ and Native Interoperability

### Deliverables

* C and C++ toolchain providers.
* Translation-unit compile actions.
* Header dependency handling.
* Static and shared libraries.
* Native tests.
* Rust-to-C/C++ linking.
* C/C++-to-Rust static or C-ABI linking.
* Runtime closure calculation.
* CMake/configure/make compatibility actions.

### Exit criteria

* A single Tong graph builds a Rust binary linked with a C++ library.
* Changing one `.cc` file rebuilds only its translation unit, affected library, and necessary downstream link actions.
* Ambient system libraries are not discovered.
* Runtime packages contain the required non-system shared-library closure.

---

## Phase 7 — Shared Cache and Remote Execution

### Deliverables

* First-class shared local-store CLI and user configuration.
* Build leases, physical-byte accounting, and store statistics.
* Reflink/unchanged-file materialization and thin-output manifests.
* Content-correct source snapshot index and bounded rustc incremental state.
* Docker executor and BuildKit cache import/export.
* Remote CAS.
* Remote action cache.
* Authentication and namespace policy.
* REAPI translation.
* Remote execution client.
* Platform-capability matching.
* Cache integrity verification.
* Upload and download concurrency control.
* `tong store export|import` bundle snapshots (seeding docker builders
  and CI with a populated store; bundle storage exists — §10).

### Exit criteria

* Two worktrees reuse identical actions and content while satisfying §10.5's
  physical-storage bound.
* The no-op and one-file-edit benchmarks satisfy §10.5's Cargo-relative gates.
* Local and Docker execution produce equivalent output-tree digests for the
  portable action suite.
* CI-produced actions are reusable by a developer with an identical execution platform.
* Corrupt remote blobs are detected.
* Local fallback works when the remote service is unavailable.
* Secrets and non-cacheable actions are never uploaded.
* Remote and local action results are semantically interchangeable.

---

## Phase 8 — Test Execution Platform

### Deliverables

* Tests as first-class actions.
* Declared test data.
* Test sharding.
* Timeout and resource policies.
* Flaky-test policy.
* Test-result caching.
* Failure-first reruns.
* Coverage provider interface.
* Structured test reports.

A test result may be cached only when all test inputs and relevant execution properties are known. Cargo’s proposed test caching follows the same fundamental requirement.

### Exit criteria

* Deterministic tests reuse cached results.
* Networked, time-sensitive, flaky, or externally dependent tests are excluded unless explicitly modeled.
* Failing tests can be rerun before the rest of the suite.

---

## Phase 9 — Backend SDK and Additional Languages

### Deliverables

* Versioned backend protocol.
* Backend conformance suite.
* Provider schema extension rules.
* Zig or Nim backend.
* JVM design prototype.
* Backend documentation.

Each backend must pass conformance tests proving:

* No ambient dependency discovery.
* No undeclared tool invocation.
* Explicit compiler identity.
* Explicit dependency artifacts.
* Network-free compilation.
* Complete declared outputs.
* Stable action construction.

---

## 16. Testing Strategy

### Unit tests

Cover:

* Canonical hashing.
* Path normalization.
* Manifest values.
* Labels.
* Platform matching.
* Toolchain selection.
* Feature propagation.
* Dependency source selection.
* Rust arguments.
* Build-script parsing.
* Provider merging.
* Cache lookup.
* GC reachability.

### Golden tests

Maintain cross-platform golden fixtures for:

* Action encodings.
* Digests.
* Lockfiles.
* Graph exports.
* Structured diagnostics.
* Toolchain fingerprints.

### Hermeticity adversarial tests

Actions should deliberately attempt to:

* Read the home directory.
* Read undeclared source files.
* Write outside outputs.
* Access the network.
* invoke an undeclared executable.
* Depend on locale.
* Depend on current time.
* Depend on workspace absolute paths.
* Leave child processes running.

### Reproducibility tests

Execute the same action:

* In different workspace paths.
* On two machines with identical toolchain bundles.
* With different parent environments.
* With different usernames.
* Across repeated clean sandboxes.

Compare output tree digests.

### Compatibility tests

For Rust:

* Compare resolved graphs with Cargo.
* Compare compiler arguments.
* Build a curated crate corpus.
* Exercise build scripts and proc macros.
* Test cross-compilation.
* Test native dependencies.
* Compare resolver 2 and resolver 3 independently; reject resolver 1.
* Keep a pinned required tier that gates releases and an extended tier that
  records evidence until promoted.
* Compare the all-platform resolved graph separately from the configured
  host/target unit graph.
* Build every required corpus entry with Tong offline after the fetch phase.

For C/C++:

* GCC, Clang, and MSVC.
* Static and dynamic linking.
* Header changes.
* Generated headers.
* Cross-language linking.
* Runtime closure behavior.

### Cache correctness tests

Cover:

* Concurrent writers.
* Interrupted writes.
* Missing outputs.
* Corrupt blobs.
* Schema upgrades.
* Remote cache poisoning.
* Toolchain changes.
* Environment changes.
* Platform mismatch.
* Docker-stage flow: a deps-only build from a manifests-only context
  (member manifests staged at their real relative paths), then a full
  build in the same store asserting every dep action was a cache hit and
  only workspace actions executed (`tong/tests/docker_stage.rs`).
* Two worktrees using one store: identical objects occupy one physical copy,
  project roots remain independent, and cleaning either worktree preserves the
  other's reachable closure.
* Interrupted builds hold leases that protect their inputs and partial
  acceleration state while never publishing a successful action result.
* Reflink/copy materialization cannot mutate a CAS blob, skips unchanged
  outputs, and removes stale selected artifacts.
* Snapshot-index false-dirty, preserved-mtime, racy-write, and corruption cases
  fall back to content hashing.
* Incremental and clean Rust compiles produce the same declared output tree for
  the reproducibility corpus.
* Docker execution denies undeclared network/filesystem access and reuses the
  same action result after workspace relocation.

---

## 17. Success Metrics

Tong should measure:

### Correctness

* Percentage of actions with enforced hermeticity.
* Sandbox violation count.
* Reproducibility verification rate.
* Cache-integrity failures.
* Undeclared dependency detections.

### Performance

* Clean-build critical path.
* No-change build latency.
* Local cache hit rate.
* Shared cache hit rate.
* Bytes uploaded and downloaded.
* Analysis time.
* Scheduler idle time.
* Peak disk usage.
* Garbage-collection effectiveness.
* Physical bytes added by a second worktree.
* Project-local `.tong` bytes versus Cargo `target` bytes.
* Reflinked, copied, shared, and reclaimable bytes.
* No-op and one-file-edit time relative to Cargo on the same pinned corpus.
* Docker context bytes, cache transfer bytes, and warm BuildKit latency.

### Compatibility

* Cargo corpus pass rate.
* Supported build-script directive coverage.
* Supported target types.
* Certified platform matrix.
* Native package compatibility.

### Developer experience

* Median time to explain a rebuild.
* Diagnostic specificity.
* Number of manual build-script permissions required.
* Migration effort per package.
* Frequency of fallback compatibility rules.

---

## 18. Principal Risks

### Cache unsoundness

The largest technical risk is returning a cached result for an action with an incomplete key.

Mitigation:

* Conservative inputs first.
* Stable hashing before shared caching.
* Mandatory sandbox enforcement.
* Adversarial tests.
* No cross-machine publication from compatibility-capture mode.

### Cargo compatibility scope

Complete Cargo command-for-command compatibility is not a target. Stable
Cargo build-workflow compatibility is a release-gated target; packaging,
publishing, installation, project generation, vendoring, and dependency
editing remain Cargo responsibilities.

Mitigation:

* Publish a compatibility matrix.
* Fail explicitly on unsupported semantics.
* Separate Cargo import from the native Tong model.
* Prioritize common monorepo workflows over obscure package-publication behavior.

### Platform asymmetry

Linux, macOS, and Windows provide different isolation primitives.

Mitigation:

* Publish enforcement levels per platform.
* Do not claim equivalent guarantees until tested.
* Keep sandbox launchers behind one interface.
* Include capability requirements in execution-platform selection.

### Native toolchain discovery

MSVC and Xcode are difficult to redistribute and fingerprint.

Mitigation:

* Separate discovery from bundle resolution.
* Import referenced files where licensing permits.
* Mark non-portable bundles clearly.
* Restrict cache sharing to matching fingerprints.

### Declarative-model escape pressure

Users will request arbitrary imperative build logic.

Mitigation:

* Provide a strictly declared `command` rule.
* Add first-class rules for repeated patterns.
* Use audited compatibility adapters.
* Keep arbitrary logic at the action boundary rather than inside analysis.

---

## 19. Definition of Tong 1.0

Tong 1.0 should not mean support for every language.

Tong 1.0 should mean:

* Rust, C, and C++ targets can coexist in one graph.
* Linux, macOS, and Windows have published certification levels.
* Builds are offline after lock and fetch.
* Compilers and SDKs are explicit toolchain closures.
* Compile and link actions have clean environments.
* Filesystem and network restrictions are enforced on certified platforms.
* Action digests are stable and cross-machine compatible.
* Local CAS, action cache, and reachability-based GC are production-ready.
* Shared remote caching is supported.
* Build scripts and proc macros are explicit, inspectable actions.
* Users can determine exactly why an action rebuilt.
* Unsupported compatibility behavior fails clearly.
* Resolver 2 and resolver 3 Cargo workflow compatibility is published and
  continuously tested; resolver 1 remains an explicit non-goal until planned.
* Every future backend is required to pass the same action-boundary conformance suite.

This establishes Tong as a credible hermetic multi-language build system rather than merely an alternative Cargo frontend.
