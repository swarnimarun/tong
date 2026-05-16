# Tong Phase 0 — Known Limitations and Unsupported Edge Cases

This document records the gaps, edge cases, and partially-implemented features that exist after the Phase 0 foundation work.  Items are grouped by subsystem and annotated with the phase that is expected to address them.

---

## 1. Cache correctness

### 1.1 Directory outputs are not verified
**What:** `ActionCache::store()` skips directories when writing the stamp, and `lookup()` also skips them when reading it back.  Build-script actions declare `out_dir` (a directory) as an output, so the cache never validates the contents of that directory.

**Impact:** If a build script generates files into `OUT_DIR` and those files change, the build-script action will still cache-hit because the directory itself is ignored.

**Mitigation:** The action re-executes if the build-script source or the script binary changes, because those are proper file inputs.

**Future work:** Phase 1 (dep-info) will replace the broad package scan with precise input lists; Phase 1 (build-script directives) will add generated files as explicit inputs.

### 1.2 Generated files inside declared output directories are invisible to the cache
**What:** When a build script writes to `OUT_DIR`, those files are collected into `generated_inputs` after the script runs, but they are only added to the *subsequent* compile action (the `rust-lib` or `rust-bin` that uses them).  They are not added to the build-script action itself.

**Impact:** Deleting a generated file will cause a cache miss for the compile action, but modifying it may not if the compile action’s other inputs haven't changed.

**Future work:** Phase 1 — add generated files as explicit inputs to the action that produces them, or hash the directory contents.

### 1.3 Missing output files cause a cache miss; extra output files do not
**What:** If an action writes an extra file that was not declared in `outputs`, the cache stamp does not record it, so a subsequent lookup will still hit even though the undeclared file exists.

**Impact:** Actions that write to unexpected paths (e.g. rustc debug symbol bundles) are not fully tracked.

**Future work:** Phase 1 — `--emit=dep-info` and discovered inputs will narrow the gap.

---

## 2. Build-script directive parsing

### 2.1 Only a subset of `cargo:` / `cargo::` directives are parsed
**Parsed today:**
- `rustc-cfg=`
- `rustc-env=`
- `rustc-link-search=`
- `rustc-link-lib=`

**Not parsed (Phase 1 work):**
- `rerun-if-changed=<path>` — changes do not invalidate the action key
- `rerun-if-env-changed=<var>` — env changes do not invalidate the action key
- `warning=` / `error=` — not surfaced to the user
- `rustc-link-arg=` / `rustc-cdylib-link-arg=` — not passed to rustc
- `rustc-flags=` — not passed to rustc

**Impact:** Build scripts that rely on rerun-if-changed will not rebuild when the watched files change. Build scripts that emit warnings or errors will silently drop them.

### 2.2 `rerun-if-changed` paths are not resolved
**What:** Even if the directive were parsed, relative paths would need to be resolved against `CARGO_MANIFEST_DIR`.

**Future work:** Phase 1 — expand directive parsing and add rerun files/env vars to action key material.

---

## 3. Build dependencies

### 3.1 Build-deps are parsed but never built
**What:** The manifest parser correctly reads `[build-dependencies]`, and `Manifest::build_dependencies` is populated, but `ProjectGraph::load()` only walks `manifest.dependencies`, not `build_dependencies`.  `RustBackend::compile_build_script()` does not compile or link build-deps before building `build.rs`.

**Impact:** A project whose `build.rs` uses a build-dependency crate will fail to compile the build script.

**Future work:** Phase 1 — build build-dependencies before the build script, pass them with `--extern`, and include host-triple in cache keys.

### 3.2 Build-deps are not separated in the feature graph
**What:** Regular dependencies and build-dependencies share the same `resolve_features()` call.  The master plan specifies separate unification scopes.

**Impact:** A feature enabled for a regular dependency could accidentally leak into the build-dep view, or vice versa.

**Future work:** Phase 1 — resolve build-deps in a separate scope.

---

## 4. Feature unification

### 4.1 Per-package resolution, not global unification
**What:** `ProjectGraph::load_manifest_recursive()` resolves features independently for each package as it is visited.  If two different paths in the graph request different features for the same shared crate, the crate is inserted into the graph once with the features from the *first* visit.

**Impact:** Diamond dependency patterns (A → C, B → C with different features) will compile C once, possibly with an incomplete feature set for one branch.

**Mitigation:** The graph does not duplicate the package node, so at least there is no double-compilation.

**Future work:** Phase 1 — resolve the entire graph first, then run a global feature-unification pass over all edges.

### 4.2 `dep:name` optional dependencies are not always excluded correctly
**What:** The feature resolver checks `optional_dependencies.contains(&dependency.alias)` and `contains(&dependency.package)`, but the alias and package names may differ (e.g. `renamed = { package = "actual", optional = true }`).  The check is permissive rather than precise.

**Future work:** Phase 1 — unify optional deps by resolved package name, not alias.

---

## 5. Workspace support

### 5.1 Members are parsed but not loaded
**What:** `WorkspaceMetadata::members` is populated from `workspace.members`, but `ProjectGraph::load()` only loads the root manifest and its recursive dependencies.  It never expands workspace member globs or loads member manifests.

**Impact:** A workspace with path-dependencies between members may fail to resolve those members because they are not part of the graph.

**Future work:** Phase 1 — expand member globs and load all workspace packages into a single graph.

### 5.2 `workspace.dependencies` are not used
**What:** `workspace.dependencies` is parsed and stored on `WorkspaceMetadata`, but `Dependency` resolution does not look up workspace-level dependency tables when a member references `workspace = true` or inherits workspace defaults.

**Future work:** Phase 1 — resolve member dependencies against the workspace root when `workspace = true` or inherited fields are present.

---

## 6. Test and example targets

### 6.1 Parsed but not built
**What:** `[[test]]` and `[[example]]` tables are parsed into `Manifest::tests` and `Manifest::examples`, and `required-features` is read, but `RustBackend::build_root()` only builds `lib` and `bin` targets.

**Impact:** `tong test`, `tong test --no-run`, and `tong build --examples` do not exist.

**Future work:** Phase 1 — add test/example compilation, skip targets whose `required-features` are not enabled.

---

## 7. Dependency-info (dep-info) input discovery

### 7.1 Not implemented
**What:** The master plan specifies `--emit=dep-info=<path>`, a `dep_info.rs` parser for Makefile-style `.d` files, and persisting discovered inputs for subsequent cache keys.  None of this is implemented.

**Current behavior:** `package_inputs()` performs a broad recursive scan of the package directory (skipping `target`, `.git`, `.jj`).  This is imprecise and slower than dep-info.

**Impact:** Cache keys include all files in the package tree, including irrelevant ones (e.g. `README.md`, test data).  They also miss files outside the tree that are `include!`d or `mod`uled via path attributes.

**Future work:** Phase 1 — add dep-info emission, parsing, and input persistence.

---

## 8. Source store and lockfiles

### 8.1 Sources use name-based directories, not content-addressed storage
**What:** `SourceFetcher::source_root()` builds paths like `{hash}-{name}`.  The hash is a key hash (name + URL + rev), not a content hash.

**Impact:** The same tarball fetched twice with different names occupies two directories.  Corruption or manual editing of a source directory is not detected.

**Future work:** Phase 2 — move to `target/tong/store/sources/blake3-<content-hash>/` and skip materialization when the hash is already present.

### 8.2 No lockfile
**What:** `Tong.lock` does not exist.  `tong fetch` materializes sources directly; `tong build` does not read a lockfile.

**Impact:** Builds are not reproducible across machines or time because source resolution is not pinned.

**Future work:** Phase 2 — define `Tong.lock` schema, separate `tong fetch` from `tong build`.

---

## 9. Metadata hash and reproducibility

### 9.1 Metadata hash is truncated to 8 hex characters
**What:** `compute_metadata_hash()` returns a BLAKE3 hash but only the first 8 characters are used in the rlib/dylib filename.

**Impact:** In theory, two different packages could produce the same 8-character prefix.  In practice, the collision probability is low (~1 in 4 billion), but it is not zero.

**Mitigation:** The full hash is stable and derived from package identity, version, features, host triple, profile, and crate kind.

**Future work:** Consider using the full 64-character hash or a 16-character prefix for stronger uniqueness.

### 9.2 `-Cmetadata` is passed but `-Cextra-filename` is not
**What:** rustc uses `-Cmetadata` for symbol disambiguation and `-Cextra-filename` for the actual file name.  Tong only passes `-Cmetadata`.

**Impact:** On some platforms or with certain crate types, the output filename may not fully disambiguate.

**Future work:** Pass `-Cextra-filename=<hash>` to ensure file names are fully unique.

### 9.3 Incremental compilation is not explicitly disabled
**What:** The master plan specified `-Cincremental=no`, but rustc interprets any value after `-Cincremental=` as a path.  Passing `-Cincremental=no` created a directory literally named `no/` in the package root, which `collect_package_files()` then picked up as an input, breaking cache stability.  The flag was removed.

**Impact:** Default rustc incremental behavior applies.  For direct rustc invocations without Cargo, incremental is typically disabled by default, but this is not guaranteed.

**Mitigation:** Setting `CARGO_INCREMENTAL=0` in the environment or using `-Z` flags would be safer; neither is currently done.

---

## 10. Cross-compilation

### 10.1 Only host compilation is supported
**What:** `RustBackend::host_triple()` returns the host triple from `rustc -Vv`.  This value is passed as both `HOST` and `TARGET` env vars to build scripts, and used in metadata hashing.  There is no `--target` flag support.

**Impact:** Cross-compilation to a different target triple is not possible.

**Future work:** Phase 3+ — add `--target` support, separate host and target toolchains, and include target triple in cache keys.

---

## 11. Environment bundles

### 11.1 Only Windows captures toolchain state
**What:** `EnvBundle::host_rust_toolchain()` returns `None` on macOS and Linux.  Only Windows captures `PATH`, `LIB`, `LIBPATH`, and `INCLUDE`.

**Impact:** On macOS, changes to the Xcode SDK path (`xcrun`) or `DEVELOPER_DIR` do not invalidate cache keys.  On Linux, Nix closures and ambient toolchain state are not fingerprinted.

**Future work:** Phase 3 — implement macOS SDK identity capture, Linux Nix closure fingerprints, and Windows toolchain bundles.

---

## 12. Manifest parsing

### 12.1 Limited semantic validation
**What:** The parser validates TOML syntax and checks for required fields (`package.name`), but does not validate:
- Duplicate target names
- Invalid edition strings
- Dependency cycles at parse time
- Missing referenced workspace members

**Impact:** Some invalid manifests will be accepted and fail later with confusing errors.

### 12.2 No typo suggestions
**What:** The master plan specifies typo detection for fields like `gitt` and `verison`.  This is not implemented.

**Impact:** Users get generic "missing key" errors instead of helpful suggestions.

**Future work:** Phase 1 — add typo detection using edit-distance on known keys.

### 12.3 `[patch]` and `[replace]` are not supported
**What:** Cargo `[patch]` and `[replace]` sections are ignored.

**Impact:** Projects that rely on these Cargo features will silently use the unpached dependency.

### 12.4 `workspace.package` inheritance is not supported
**What:** Fields like `version.workspace = true` or `edition.workspace = true` are not resolved.

**Impact:** Workspaces that use package inheritance will fail to parse.

---

## 13. GC limitations

### 13.1 Only explicitly tracked files are kept alive
**What:** `BuildState` records `outputs`, `stdouts`, `stamps`, `dep_infos`, and `sources` from the executor.  Files created by actions but not declared as outputs (e.g. debug symbol bundles, `.d` dep-info files) are not tracked.

**Impact:** `tong gc` may delete undeclared but useful files.  `tong gc --dry-run` should be used to audit before deletion.

**Mitigation:** `tong clean` remains the safe full-reset command.

### 13.2 Directories are not tracked
**What:** `BuildState` records file paths only.  Empty directories that were created by actions are not tracked and may be removed by GC.

**Impact:** Minimal — empty directories have no semantic content.

---

## 14. Linking and runtime

### 14.1 No RUNPATH / rpath handling
**What:** Native shared libraries are not tracked, and `rustc` is not told where to find them at runtime.

**Impact:** Binaries that depend on native `.so` / `.dylib` libraries may fail to run unless the libraries are in the system search path.

**Future work:** Phase 4.

### 14.2 No DLL closure computation
**What:** On Windows, required DLLs are not copied next to the binary.

**Future work:** Phase 4.

---

## 15. Sandboxing

### 15.1 No sandboxing
**What:** Actions run with the full privileges of the invoking process and have unrestricted filesystem and network access.

**Impact:** Malicious or buggy build scripts can modify files outside the project, access the network, or read sensitive files.

**Future work:** Phase 5 — implement opt-in platform sandbox backends.

---

## 16. Networking

### 16.1 `tong fetch` does not materialize registry dependencies
**What:** `tong fetch` resolves path dependencies and prints a count, but registry dependencies (e.g. `serde = "1.0"`) require a source override in `[tong.sources]` or fail.

**Impact:** Projects with registry dependencies cannot build unless every registry dep has an explicit source override.

**Future work:** Phase 2 — add crates.io sparse-index resolution.

### 16.2 `tong update` does not exist
**What:** There is no command to refresh resolutions and rewrite `Tong.lock`.

**Future work:** Phase 2.

---

## Summary Table

| Category | Limitation | Phase |
|---|---|---|
| Cache | Directory outputs not verified | 1 |
| Cache | Generated files not tracked in producer action | 1 |
| Build script | Missing rerun-if-changed/env-changed support | 1 |
| Build script | Missing warning/error/link-arg directives | 1 |
| Build deps | Parsed but not built | 1 |
| Build deps | No separate feature scope | 1 |
| Features | Per-package resolution, not global | 1 |
| Workspace | Members not loaded | 1 |
| Workspace | workspace.dependencies not used | 1 |
| Tests/Examples | Parsed but not built | 1 |
| Dep-info | Not implemented | 1 |
| Source store | Name-based, not content-addressed | 2 |
| Lockfile | Does not exist | 2 |
| Registry | No crates.io resolution | 2 |
| Metadata hash | 8-char prefix collision possible | — |
| Incremental | Not explicitly disabled | — |
| Cross-compile | Only host triple | 3+ |
| Env bundles | macOS/Linux not captured | 3 |
| Manifest | No typo suggestions | 1 |
| Manifest | patch/replace not supported | — |
| Manifest | workspace.package inheritance not supported | 1 |
| GC | Undeclared files may be deleted | — |
| Linking | No RUNPATH/DLL handling | 4 |
| Sandbox | Not implemented | 5 |
