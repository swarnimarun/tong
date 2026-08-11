//! Action planning for the Rust backend.
//!
//! Lowers the [`RustModel`] into [`PlannedAction`]s. Every crate compilation
//! — library, binary, proc macro, build script — becomes one action whose
//! executable is the captured rustc (PLAN.md section 3.1: backends never
//! invoke compilers as ambient subprocesses; compilation only happens inside
//! actions). Specs are concretized at schedule time because dependency
//! artifact digests are only known after their producers run (section 8.3).
//!
//! The toolchain and sysroot are system-captured and non-portable; action
//! properties record `rustc -vV` and the sysroot tree digest so toolchain
//! changes invalidate caches (sections 4.3, 5, 8.4).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use tong_core::action::{
    ACTION_SCHEMA_VERSION, ActionId, ActionSpec, Argument, CachePolicy, CanonicalValue,
    NetworkPolicy, ResourceRequirements,
};
use tong_core::artifact::{ArtifactRef, BlobDigest, TreeDigest};
use tong_core::bundle::BundleRef;
use tong_core::paths::{OutputPath, RelativePath};
use tong_core::tree::{Tree, TreeEntry};
use tong_exec::EXEC_ROOT_VAR;
use tong_graph::{Completed, PlanError, PlannedAction};
use tong_store::{CAPTURE_EXCLUDES, Cas};

use crate::build_directives::{Directives, parse_directives};
use crate::model::{
    CrateType, Dep, Edition, Package, PackageId, ProfileSpec, RustModel, RustUnitDomain,
    RustUnitId, RustUnitMode, TestTarget, crate_name, lib_crate_name,
};
use crate::toolchain::{SystemRust, dll_extension, host_platform};

/// A final runnable artifact produced by the build.
#[derive(Clone, Debug)]
pub struct FinalArtifact {
    /// Output name (the binary file name).
    pub name: String,
    /// Action whose output tree contains the binary.
    pub action: ActionId,
    /// Binary path within the action's output tree.
    pub output: OutputPath,
    /// Runtime files to place next to the binary.
    pub runtime: Vec<(BlobDigest, String)>,
}

/// An imported native library, plan-time data.
#[derive(Clone, Debug)]
struct CcInfo {
    /// Tree containing the shared library file.
    tree: TreeDigest,
    /// File name inside the tree.
    file_name: String,
    /// The library blob itself.
    blob: BlobDigest,
    /// `-l` link name.
    link_name: String,
}

/// A dependency resolved at plan time.
#[derive(Clone, Debug)]
enum DepSpec {
    /// A Rust crate produced by another action.
    Rust {
        /// `--extern` name.
        extern_name: String,
        /// Producer action.
        action: ActionId,
        /// Artifact file name (e.g. `libfoo-0123456789abcdef.rlib`).
        file: String,
    },
    /// A prebuilt native library (plan-time, no action).
    Native,
}

/// Data captured at plan time for one action, concretized at schedule time.
struct Ctx {
    logical_id: ActionId,
    mnemonic: String,
    /// Whether the owning package is outside the workspace (a dependency):
    /// `tong build --deps-only` executes only external actions.
    external: bool,
    kind: CtxKind,
    source_tree: TreeDigest,
    rustc: BlobDigest,
    bundle: Option<BundleRef>,
    properties: BTreeMap<String, CanonicalValue>,
    global_env: BTreeMap<String, String>,
    pkg_env: BTreeMap<String, String>,
    cc: Vec<(String, TreeDigest, String)>,
    profile_flags: Vec<String>,
    /// `[policy] network = "allow"`: run actions may reach the network
    /// and are uncacheable.
    network_allow: bool,
    /// Configured target triple for target units (`--target`); `None`
    /// keeps everything host.
    target_triple: Option<String>,
}

enum CtxKind {
    Compile(CompileSpec),
    BuildScriptRun(BuildScriptRunSpec),
    TestRun(TestRunSpec),
}

struct CompileSpec {
    crate_name: String,
    edition: Edition,
    crate_type: String,
    meta: String,
    output: String,
    deps: Vec<DepSpec>,
    build_script: Option<ActionId>,
    /// Build-script runs whose link directives apply to this crate: the
    /// crate's own script plus those of its transitive dependencies
    /// (Cargo semantics — `cargo:rustc-link-lib`, `rustc-link-search`,
    /// `rustc-flags`, and `rustc-env` propagate to every dependent).
    directive_sources: Vec<ActionId>,
    crate_root: PathBuf,
    extra_flags: Vec<String>,
    /// `--cfg feature="..."` flags for the package's activated features.
    feature_cfgs: Vec<String>,
    /// Direct deps plus the transitive closure (all mounted at `deps/`).
    transitive_deps: Vec<DepSpec>,
    /// The package's library is a proc macro (test compiles need
    /// `--extern proc_macro` too).
    is_proc_macro: bool,
    /// Host unit (build script / proc macro): never `--target`, uses the
    /// execution triple.
    host_unit: bool,
    /// Workspace member (sets `CARGO_PRIMARY_PACKAGE`).
    primary: bool,
    /// `--check` build: rustc emits metadata only.
    check: bool,
    /// Package binaries exposed to integration tests as
    /// `CARGO_BIN_EXE_<target>`.
    bin_env: Vec<(String, ActionId, String)>,
}

struct BuildScriptRunSpec {
    compile: ActionId,
    /// Direct dependencies' `links` values → their build-script run ids;
    /// their metadata is exported as `DEP_<LINKS>_<KEY>` to this script.
    dep_links: Vec<(String, ActionId)>,
    binary: String,
    pkg_name: String,
    pkg_version: String,
    host_triple: String,
    target_triple: String,
    opt_level: String,
    debug: bool,
    /// The real rustc path (build scripts expect `$RUSTC`, cargo sets it).
    rustc_path: PathBuf,
    rustdoc_path: PathBuf,
    encoded_rustflags: String,
    links: Option<String>,
    /// Profile name (`$PROFILE`).
    profile: String,
    /// `CARGO_CFG_*` values computed from the configured target triple.
    cfgs: Vec<(String, String)>,
    /// The package's activated features (`CARGO_FEATURE_*`).
    features: Vec<String>,
}

struct TestRunSpec {
    compile: ActionId,
    binary: String,
    harness: bool,
    args: Vec<String>,
    /// `cache-test-result = true` (native) makes the run cacheable.
    cache_test_result: bool,
    /// Doc test: executed by rustdoc (`rustdoc --test`), no separate
    /// compile; `deps` are the linked crates and `crate_root`/`edition`
    /// the doctest inputs.
    doc: bool,
    deps: Vec<DepSpec>,
    crate_root: PathBuf,
    edition: Edition,
    /// Package binary actions mounted at the same relative paths embedded
    /// by `CARGO_BIN_EXE_*` during integration-test compilation.
    bin_env: Vec<(String, ActionId, String)>,
}

/// The Rust backend: plans actions from a [`RustModel`].
pub struct RustBackend<'a> {
    cas: Cas,
    model: &'a RustModel,
    toolchain: SystemRust,
    profile_name: String,
    profile: ProfileSpec,
    /// Per-package profile overrides for the active profile name.
    package_profiles: BTreeMap<String, ProfileSpec>,
    /// Packages compiled in the resolver's host feature domain because
    /// they are build dependencies or dependencies of proc macros.
    host_packages: BTreeSet<PackageId>,
    /// Packages compiled in the configured target feature domain.
    target_packages: BTreeSet<PackageId>,
    /// Packages reachable from the configured roots through active
    /// dependency edges. Lock-only packages never become build units.
    configured_packages: BTreeSet<PackageId>,
    /// Target-domain libraries that must remain linkable during `check`.
    /// Proc-macro unit tests load their dependency closure as rlibs even
    /// when rustc emits metadata for ordinary check units.
    full_codegen_packages: BTreeSet<PackageId>,
    source_trees: BTreeMap<PackageId, TreeDigest>,
    /// Original crate path (relative to the package dir) → rewritten path
    /// inside the source tree, for crate roots mounted outside the package
    /// dir.
    crate_roots: BTreeMap<(PackageId, PathBuf), PathBuf>,
    cc: BTreeMap<String, CcInfo>,
    cc_closure: BTreeMap<PackageId, Vec<String>>,
    planned_ids: BTreeMap<String, ActionId>,
    /// Whether test targets are planned and run (`tong test`).
    tests_enabled: bool,
    /// Also plan examples, benches, and doc tests (test/`--all-targets`
    /// builds; plain `tong build` skips them like cargo).
    all_targets: bool,
    /// `--no-run`: plan test compiles without their run actions.
    no_run: bool,
    /// `--check`-style builds: rustc emits metadata only.
    check: bool,
    /// `[policy] network = "allow"`: run actions may reach the network
    /// and are uncacheable.
    network_allow: bool,
    /// Captured rustdoc blob (doc tests), captured on first use.
    rustdoc_blob: Option<BlobDigest>,
    /// Configured target triple for target units (`--target`); `None`
    /// keeps everything host.
    target_triple: Option<String>,
    /// Arguments passed to the test binaries (after `--`).
    test_args: Vec<String>,
    /// Build-state store, for rerun-if-changed input narrowing (the
    /// previous run's directives).
    state: Option<tong_store::StateStore>,
    /// The workspace's project hash (state lookup key).
    project_hash: Option<tong_core::digest::Digest>,
}

impl<'a> RustBackend<'a> {
    /// Creates a backend using `profile_name` from the model's profile table.
    pub fn new(
        cas: Cas,
        model: &'a RustModel,
        toolchain: SystemRust,
        profile_name: &str,
    ) -> Result<Self, PlanError> {
        Self::with_state(cas, model, toolchain, profile_name, None, None)
    }

    /// Creates a backend with build-state access (for `rerun-if-changed`
    /// input narrowing).
    pub fn with_state(
        cas: Cas,
        model: &'a RustModel,
        toolchain: SystemRust,
        profile_name: &str,
        state: Option<tong_store::StateStore>,
        project_hash: Option<tong_core::digest::Digest>,
    ) -> Result<Self, PlanError> {
        Self::with_tests_state(
            cas,
            model,
            toolchain,
            profile_name,
            false,
            &[],
            state,
            project_hash,
            false,
            false,
            false,
            false,
            None,
        )
    }

    /// Creates a backend; `tests_enabled` plans and runs test targets,
    /// `test_args` are passed to the test binaries.
    pub fn with_tests(
        cas: Cas,
        model: &'a RustModel,
        toolchain: SystemRust,
        profile_name: &str,
        tests_enabled: bool,
        test_args: &[String],
    ) -> Result<Self, PlanError> {
        Self::with_tests_state(
            cas,
            model,
            toolchain,
            profile_name,
            tests_enabled,
            test_args,
            None,
            None,
            false,
            tests_enabled,
            false,
            false,
            None,
        )
    }

    /// Full constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn with_tests_state(
        cas: Cas,
        model: &'a RustModel,
        toolchain: SystemRust,
        profile_name: &str,
        tests_enabled: bool,
        test_args: &[String],
        state: Option<tong_store::StateStore>,
        project_hash: Option<tong_core::digest::Digest>,
        network_allow: bool,
        all_targets: bool,
        no_run: bool,
        check: bool,
        target_triple: Option<String>,
    ) -> Result<Self, PlanError> {
        let profile = model
            .profiles
            .get(profile_name)
            .cloned()
            .ok_or_else(|| PlanError::Message(format!("unknown profile {profile_name:?}")))?;
        let build_host_triple = toolchain.host_triple.clone();
        let configured_triple = target_triple
            .as_deref()
            .unwrap_or(&build_host_triple)
            .to_owned();
        let (configured_packages, target_packages, host_packages) = configured_package_domains(
            model,
            tests_enabled || all_targets,
            &configured_triple,
            &build_host_triple,
        );
        let full_codegen_packages = if check && (tests_enabled || all_targets) {
            proc_macro_test_codegen_closure(model, &configured_triple)
        } else {
            BTreeSet::new()
        };
        Ok(Self {
            cas,
            model,
            toolchain,
            profile_name: profile_name.to_owned(),
            profile,
            package_profiles: model
                .package_profiles
                .iter()
                .filter(|((name, _), _)| name == profile_name)
                .map(|((_, spec), profile)| (spec.clone(), profile.clone()))
                .collect(),
            host_packages,
            target_packages,
            configured_packages,
            full_codegen_packages,
            source_trees: BTreeMap::new(),
            crate_roots: BTreeMap::new(),
            cc: BTreeMap::new(),
            cc_closure: BTreeMap::new(),
            planned_ids: BTreeMap::new(),
            tests_enabled,
            all_targets,
            no_run,
            check,
            network_allow,
            rustdoc_blob: None,
            target_triple,
            test_args: test_args.to_vec(),
            state,
            project_hash,
        })
    }

    /// Captures inputs and produces the planned action graph.
    pub fn plan(&mut self) -> Result<Vec<PlannedAction>, PlanError> {
        // 1. Capture package source trees once (PLAN.md section 8.3: whole
        //    package tree, excluding known output directories).
        for pkg in &self.model.packages {
            if !self.configured_packages.contains(&pkg.id) {
                continue;
            }
            let mut excludes = CAPTURE_EXCLUDES.iter().copied().collect();
            let mut tree = self.cas.capture_dir_filtered(&pkg.dir, &excludes)?;

            // Some archive/cache transports materialize a Git symlink as
            // a small text file containing its relative target. Preserve
            // the checkout semantics without copying the whole workspace:
            // replace a top-level `src` placeholder with the target tree,
            // which the CAS deduplicates by content.
            let src = pkg.dir.join("src");
            if src.is_file()
                && let Ok(link) = fs::read_to_string(&src)
            {
                let link = link.trim();
                let target = pkg.dir.join(link);
                if !link.is_empty() && !Path::new(link).is_absolute() && target.is_dir() {
                    excludes.insert("src");
                    tree = self.cas.capture_dir_filtered(&pkg.dir, &excludes)?;
                    let target_tree = self.cas.capture_dir_filtered(&target, &excludes)?;
                    tree = self.cas.assemble(&[
                        (RelativePath::new(".").unwrap(), tree),
                        (RelativePath::new("src").unwrap(), target_tree),
                    ])?;
                }
            }

            // Native packages replace the raw `Tong.toml` with its
            // canonical, label-independent rendering: `[target.<key>]`
            // table renames (graph labels) are digest-neutral as long as
            // the stable fields stay unchanged.
            if pkg.dir.join("Tong.toml").is_file() {
                excludes.insert("Tong.toml");
                tree = self.cas.capture_dir_filtered(&pkg.dir, &excludes)?;
                let blob = self.cas.put_blob(pkg.canonical_manifest().as_bytes())?;
                let manifest_tree = Tree::new(
                    [(
                        "Tong.toml".to_owned(),
                        TreeEntry::File {
                            digest: blob,
                            executable: false,
                        },
                    )]
                    .into_iter()
                    .collect(),
                )
                .map_err(|err| PlanError::Message(format!("invalid tree: {err}")))?;
                let manifest_tree = self.cas.put_tree(&manifest_tree)?;
                tree = self.cas.assemble(&[
                    (RelativePath::new(".").unwrap(), tree),
                    (RelativePath::new("Tong.toml").unwrap(), manifest_tree),
                ])?;
            }

            // Tong.toml targets may reference crate roots outside the
            // package dir (e.g. a shared bindings crate in another
            // example). Mount each external crate's parent directory into
            // the source tree at `ext/<n>` and rewrite the root.
            let mut external: Vec<PathBuf> = Vec::new();
            if let Some(lib) = &pkg.lib {
                external.push(lib.path.clone());
            }
            for bin in &pkg.bins {
                external.push(bin.path.clone());
            }
            if let Some(script) = &pkg.build_script {
                external.push(script.clone());
            }
            let pkg_dir = fs::canonicalize(&pkg.dir)?;
            let mut index = 0usize;
            for path in external {
                let full = pkg.dir.join(&path);
                if !full.exists() {
                    continue;
                }
                let canonical = fs::canonicalize(&full)?;
                if canonical.starts_with(&pkg_dir) {
                    continue;
                }
                let parent = canonical.parent().ok_or_else(|| {
                    PlanError::Message(format!(
                        "cannot mount external crate root {}",
                        full.display()
                    ))
                })?;
                let ext_tree = self.cas.capture_dir_filtered(parent, &excludes)?;
                let mount = RelativePath::new(&format!("ext/{index}"))
                    .map_err(|err| PlanError::Message(format!("invalid mount: {err}")))?;
                tree = self
                    .cas
                    .assemble(&[(RelativePath::new(".").unwrap(), tree), (mount, ext_tree)])
                    .map_err(|error| {
                        PlanError::Message(format!(
                            "cannot mount external crate root {} for {}: {error}",
                            canonical.display(),
                            pkg.id
                        ))
                    })?;
                let relative = canonical.strip_prefix(parent).map_err(|_| {
                    PlanError::Message(format!(
                        "cannot relativize external crate root {}",
                        canonical.display()
                    ))
                })?;
                let rewritten = PathBuf::from(format!("ext/{index}")).join(relative);
                self.crate_roots.insert((pkg.id.clone(), path), rewritten);
                index += 1;
            }

            self.source_trees.insert(pkg.id.clone(), tree);
        }

        // 2. Import prebuilt native libraries.
        for import in &self.model.cc_imports {
            self.import_cc(import)?;
        }

        // 3. Compute transitive native closures per package.
        let pkg_map: BTreeMap<PackageId, &Package> = self
            .model
            .packages
            .iter()
            .map(|pkg| (pkg.id.clone(), pkg))
            .collect();
        for pkg in &self.model.packages {
            if !self.configured_packages.contains(&pkg.id) {
                continue;
            }
            let closure = self.collect_cc(pkg, &pkg_map);
            self.cc_closure.insert(pkg.id.clone(), closure);
        }

        // 4. Build-script action ids are deterministic
        //    (`rust:bs-run:<pkg>`); pre-register them (for packages that
        //    have a build script) so any package can reference its
        //    dependencies' build-script directives while planning,
        //    regardless of package order.
        for pkg in &self.model.packages {
            if !self.configured_packages.contains(&pkg.id) {
                continue;
            }
            if pkg.build_script.is_some() {
                if self.target_packages.contains(&pkg.id) {
                    self.planned_ids.insert(
                        format!("bs-run:{}", self.pkg_key(pkg)),
                        ActionId(format!("rust:bs-run:{}", self.pkg_label(pkg))),
                    );
                }
                if self.host_packages.contains(&pkg.id) {
                    self.planned_ids.insert(
                        format!("bs-run:{}:host", self.pkg_key(pkg)),
                        ActionId(format!("rust:bs-run:{}:host", self.pkg_label(pkg))),
                    );
                }
            }
        }

        // 4b. Library action ids are deterministic too; pre-register them
        //     so a library depending on another library (e.g. core →
        //     unixonly, alphabetically later) resolves its dependency
        //     actions regardless of package order.
        for pkg in &self.model.packages {
            if !self.configured_packages.contains(&pkg.id) {
                continue;
            }
            if let Some(lib) = &pkg.lib {
                if lib.proc_macro {
                    self.planned_ids.insert(
                        self.lib_key(pkg, "proc-macro", true),
                        ActionId(format!("rust:proc-macro:{}", self.pkg_label(pkg))),
                    );
                } else {
                    let types: Vec<CrateType> = if lib.crate_types.is_empty() {
                        vec![CrateType::Rlib]
                    } else {
                        lib.crate_types.clone()
                    };
                    for crate_type in types {
                        if self.target_packages.contains(&pkg.id) {
                            self.planned_ids.insert(
                                self.lib_key(pkg, crate_type.to_rustc(), false),
                                ActionId(format!(
                                    "rust:lib:{}:{}",
                                    self.pkg_label(pkg),
                                    crate_type.to_rustc()
                                )),
                            );
                        }
                        if self.host_packages.contains(&pkg.id) {
                            self.planned_ids.insert(
                                self.lib_key(pkg, crate_type.to_rustc(), true),
                                ActionId(format!(
                                    "rust:lib:{}:{}:host",
                                    self.pkg_label(pkg),
                                    crate_type.to_rustc()
                                )),
                            );
                        }
                    }
                }
            }
        }

        // 5. Plan actions. Libraries, proc macros, and build scripts first
        //    (their planned ids must exist before binaries resolve their
        //    dependency actions), then binaries, then tests.
        let mut actions = Vec::new();
        for pkg in &self.model.packages {
            if !self.configured_packages.contains(&pkg.id) {
                continue;
            }
            self.plan_package_library(&mut actions, pkg)?;
        }
        for pkg in &self.model.packages {
            if !self.configured_packages.contains(&pkg.id) {
                continue;
            }
            self.plan_package_bins(&mut actions, pkg)?;
        }
        if self.all_targets {
            for pkg in &self.model.packages {
                if !self.configured_packages.contains(&pkg.id) {
                    continue;
                }
                self.plan_package_examples(&mut actions, pkg)?;
            }
        }
        if self.tests_enabled {
            for pkg in &self.model.packages {
                if !self.configured_packages.contains(&pkg.id) {
                    continue;
                }
                self.plan_package_tests(&mut actions, pkg)?;
            }
        }

        Ok(actions)
    }

    /// Plans a package's test targets: one compile action per target
    /// (`rustc --test`, deps + dev-deps + the package's own lib) and one
    /// uncached run action executing the test binary.
    fn plan_package_tests(
        &mut self,
        actions: &mut Vec<PlannedAction>,
        pkg: &Package,
    ) -> Result<(), PlanError> {
        let source_tree = self.source_trees[&pkg.id];
        let cc = self.cc_for(&pkg.id);
        let bs_run: Option<ActionId> = self
            .planned_ids
            .get(&format!("bs-run:{}", self.pkg_key(pkg)))
            .cloned();

        if !self.is_selected_member(pkg) {
            return Ok(());
        }
        for target in &pkg.tests {
            let selection_kind = if target.bench { "bench" } else { "test" };
            if !self.target_selected(selection_kind, &target.name) {
                continue;
            }
            if !self.required_features_active(pkg, &target.required_features) {
                continue;
            }
            if target.doc {
                // Doc tests: one rustdoc --test run action; rustdoc
                // compiles the crate itself, so no separate compile.
                if self.no_run || self.check {
                    continue;
                }
                self.plan_doc_test(actions, pkg, target, source_tree, cc.clone())?;
                continue;
            }
            // Cargo permits an integration test to have the same name as
            // the package's library unit-test target. Keep the CLI selector
            // as `test`, but encode the concrete target kind in unit ids so
            // the two compilations and runs cannot alias.
            let unit_kind =
                if target.bench {
                    "bench"
                } else if pkg.lib.as_ref().is_some_and(|lib| {
                    lib.path == target.path && lib_crate_name(pkg) == target.name
                }) {
                    "lib"
                } else {
                    "test"
                };
            // deps + dev-deps + the package's own library.
            let mut deps = pkg.deps.clone();
            deps.extend(pkg.dev_deps.iter().cloned());
            if pkg.lib.is_some() {
                deps.insert(
                    0,
                    Dep {
                        extern_name: lib_crate_name(pkg),
                        package: pkg.id.clone(),
                        optional: false,
                        default_features: true,
                        features: Vec::new(),
                        target: None,
                    },
                );
            }
            let crate_name = crate_name(&target.name);
            // A proc-macro package's unit tests reuse the proc-macro's host
            // dependency closure. Even `cargo check --all-targets` needs
            // those dependencies as linkable rlibs; target-domain `.rmeta`
            // files are insufficient when rustc loads the macro crate.
            let compile_id = self.plan_compile(
                actions,
                &format!(
                    "test-compile:{}:{unit_kind}:{}",
                    self.pkg_key(pkg),
                    target.name
                ),
                &format!(
                    "rust:test-compile:{}:{unit_kind}:{}",
                    self.pkg_label(pkg),
                    target.name
                ),
                "RustTestCompile",
                pkg,
                source_tree,
                cc.clone(),
                crate_name,
                "test",
                Some(target.name.clone()),
                &deps,
                bs_run.clone(),
                self.crate_root_for(&pkg.id, &target.path),
                false,
                false,
                true,
            )?;

            if self.no_run || self.check {
                continue;
            }
            let run_id = ActionId(format!(
                "rust:test-run:{}:{unit_kind}:{}",
                self.pkg_label(pkg),
                target.name
            ));
            let run_ctx = Ctx {
                logical_id: run_id.clone(),
                mnemonic: "RustTestRun".to_owned(),
                external: self.pkg_external(pkg),
                kind: CtxKind::TestRun(TestRunSpec {
                    compile: compile_id.clone(),
                    binary: target.name.clone(),
                    harness: target.harness,
                    args: self.test_args.clone(),
                    cache_test_result: target.cache_test_result,
                    doc: false,
                    deps: Vec::new(),
                    crate_root: PathBuf::new(),
                    edition: pkg.edition,
                    bin_env: self.package_bin_env(pkg),
                }),
                source_tree,
                rustc: self.toolchain.rustc_blob,
                bundle: Some(self.toolchain.bundle_ref()),
                properties: self.base_properties(),
                global_env: self.model.global_env.clone(),
                pkg_env: self.pkg_cargo_env(pkg),
                cc: Vec::new(),
                profile_flags: Vec::new(),
                network_allow: self.network_allow,
                target_triple: self.target_triple.clone(),
            };
            actions.push(self.boxed(run_ctx));
            let _ = &run_id;
        }
        Ok(())
    }

    /// Plans a doc-test run: `rustdoc --test <crate-root>` with the
    /// package's library and dependencies linked.
    fn plan_doc_test(
        &mut self,
        actions: &mut Vec<PlannedAction>,
        pkg: &Package,
        target: &TestTarget,
        source_tree: TreeDigest,
        _cc: Vec<(String, TreeDigest, String)>,
    ) -> Result<(), PlanError> {
        let rustdoc = self
            .toolchain
            .rustc
            .parent()
            .unwrap_or(Path::new("."))
            .join("rustdoc");
        if !rustdoc.is_file() {
            return Err(PlanError::Message(format!(
                "doc test `{}` requires rustdoc, which this toolchain does not ship",
                target.name
            )));
        }
        let rustdoc_blob = match self.rustdoc_blob {
            Some(blob) => blob,
            None => {
                let blob = self.cas.put_file(&rustdoc)?;
                self.rustdoc_blob = Some(blob);
                blob
            }
        };
        // deps + dev-deps + the package's own library.
        let mut deps = pkg.deps.clone();
        deps.extend(pkg.dev_deps.iter().cloned());
        if pkg.lib.is_some() {
            deps.insert(
                0,
                Dep {
                    extern_name: lib_crate_name(pkg),
                    package: pkg.id.clone(),
                    optional: false,
                    default_features: true,
                    features: Vec::new(),
                    target: None,
                },
            );
        }
        let dep_specs = self.resolve_deps(&pkg.id, &deps, false)?;
        let transitive = self.transitive_dep_specs(&pkg.id, &deps, false)?;
        let run_id = ActionId(format!(
            "rust:doc-test:{}:{}",
            self.pkg_label(pkg),
            target.name
        ));
        let run_ctx = Ctx {
            logical_id: run_id.clone(),
            mnemonic: "RustDocTest".to_owned(),
            external: self.pkg_external(pkg),
            kind: CtxKind::TestRun(TestRunSpec {
                compile: ActionId(String::new()),
                binary: target.name.clone(),
                harness: false,
                args: self.test_args.clone(),
                cache_test_result: false,
                doc: true,
                deps: dep_specs,
                crate_root: self.crate_root_for(&pkg.id, &target.path),
                edition: pkg.edition,
                bin_env: self.package_bin_env(pkg),
            }),
            source_tree,
            rustc: rustdoc_blob,
            bundle: Some(self.toolchain.bundle_ref()),
            properties: self.base_properties(),
            global_env: self.model.global_env.clone(),
            pkg_env: {
                let mut env = self.pkg_cargo_env(pkg);
                env.insert("CARGO_CRATE_NAME".to_owned(), lib_crate_name(pkg));
                env.insert("CARGO_PRIMARY_PACKAGE".to_owned(), "1".to_owned());
                env
            },
            cc: Vec::new(),
            profile_flags: Vec::new(),
            network_allow: self.network_allow,
            target_triple: self.target_triple.clone(),
        };
        // The run depends on the transitive closure (rlibs at deps/).
        let mut all_deps: Vec<ActionId> = transitive
            .iter()
            .filter_map(|dep| match dep {
                DepSpec::Rust { action, .. } => Some(action.clone()),
                DepSpec::Native => None,
            })
            .collect();
        all_deps.sort();
        all_deps.dedup();
        let mut action = self.boxed(run_ctx);
        action.deps = all_deps;
        actions.push(action);
        Ok(())
    }

    /// The previous build's dep-info inputs for a compile action: the
    /// `.d` file in its recorded output tree, relativized to the package
    /// dir. `None` when there is no previous record, the dep-info is
    /// missing or malformed, or it names a path outside the package
    /// (keeping the conservative whole-tree input).
    fn previous_dep_info(&self, logical_id: &str, pkg: &Package) -> Option<Vec<PathBuf>> {
        let state = self.state.as_ref()?;
        let project_hash = self.project_hash?;
        let manifest = state.latest(&project_hash)?;
        let action = manifest
            .actions
            .iter()
            .find(|action| action.logical_id == logical_id)?;
        let tree = self.cas.get_tree(action.outputs).ok().flatten()?;
        let dep_text = find_dep_blob(&tree, &self.cas)?;
        let text = String::from_utf8_lossy(&dep_text);
        let pkg_dir = fs::canonicalize(&pkg.dir).ok()?;
        let mut paths = Vec::new();
        for path in parse_dep_info(&text) {
            let Ok(relative) = path.strip_prefix(&pkg_dir) else {
                return None;
            };
            if !relative.as_os_str().is_empty() {
                paths.push(relative.to_path_buf());
            }
        }
        (!paths.is_empty()).then_some(paths)
    }

    /// The previous successful run's directives for a package's build
    /// script, read from the build-state manifest (the latest successful
    /// graph). `None` on the first build.
    fn previous_directives(&self, pkg: &Package, host_domain: bool) -> Option<Directives> {
        let state = self.state.as_ref()?;
        let project_hash = self.project_hash?;
        let manifest = state.latest(&project_hash)?;
        let suffix = if host_domain { ":host" } else { "" };
        let id = format!("rust:bs-run:{}{suffix}", self.pkg_label(pkg));
        let action = manifest
            .actions
            .iter()
            .find(|action| action.logical_id == id)?;
        let stdout = self.cas.read_blob(action.stdout).ok()?;
        Some(parse_directives(&String::from_utf8_lossy(&stdout)))
    }

    /// Builds the build-script run action's source tree according to the
    /// previous run's `rerun-if-changed` directives: only the declared
    /// paths (plus the build script itself, which Cargo always tracks) are
    /// captured; a declared path that no longer exists is an error, like
    /// Cargo. Without any directives the whole package tree is kept
    /// (Cargo's "rerun if anything changes" fallback).
    fn narrowed_script_tree(
        &self,
        pkg: &Package,
        full: TreeDigest,
        directives: &Directives,
    ) -> Result<TreeDigest, PlanError> {
        if directives.rerun_if_changed.is_empty() {
            return Ok(full);
        }
        // System-library probes commonly report absolute header
        // directories (for example Homebrew OpenSSL). They are not part of
        // the package source tree and cannot safely be remounted under a
        // different path. Keep the conservative whole-package input until
        // system dependency capture can model those paths explicitly.
        if directives
            .rerun_if_changed
            .iter()
            .any(|path| Path::new(path).is_absolute() || !pkg.dir.join(path).exists())
        {
            return Ok(full);
        }
        let mut paths: Vec<PathBuf> = directives
            .rerun_if_changed
            .iter()
            .map(PathBuf::from)
            .collect();
        // Cargo always reruns when the build script itself changes.
        if let Some(script) = &pkg.build_script {
            let script = self.crate_root_for(&pkg.id, script);
            if !paths.contains(&script) {
                paths.push(script.clone());
            }
        }
        self.mount_package_paths(pkg, paths, "rerun-if-changed")
    }

    /// Builds a source tree containing exactly the given package-relative
    /// paths (files and directories), plus the package manifest.
    fn mount_package_paths(
        &self,
        pkg: &Package,
        mut paths: Vec<PathBuf>,
        what: &str,
    ) -> Result<TreeDigest, PlanError> {
        let pkg_dir = fs::canonicalize(&pkg.dir)?;
        let excludes: std::collections::BTreeSet<&str> = CAPTURE_EXCLUDES.iter().copied().collect();
        let mut mounts: Vec<(RelativePath, TreeDigest)> = Vec::new();
        let mut mounted: std::collections::BTreeSet<PathBuf> = Default::default();
        // Capture parent directories before nested files. A rustc dep-info
        // file may name both `src/` and `src/main.rs`; mounting both would
        // attempt to overlay the same entry in the immutable tree.
        paths.sort_by_key(|path| path.components().count());
        for path in paths {
            let full_path = pkg.dir.join(&path);
            if !full_path.exists() {
                return Err(PlanError::Message(format!(
                    "{what} path {:?} of package {} no longer exists",
                    path.display(),
                    pkg.name
                )));
            }
            let canonical = fs::canonicalize(&full_path)?;
            let relative = canonical.strip_prefix(&pkg_dir).map_err(|_| {
                PlanError::Message(format!(
                    "{what} path {:?} of package {} lies outside the package \
                     directory; only in-package paths are supported",
                    path.display(),
                    pkg.name
                ))
            })?;
            if mounted.iter().any(|root| relative.starts_with(root)) {
                continue;
            }
            let mount_path = RelativePath::new(&relative.to_string_lossy()).map_err(|err| {
                PlanError::Message(format!("invalid {what} path {:?}: {err}", path))
            })?;
            if canonical.is_dir() {
                if mounted.insert(relative.to_path_buf()) {
                    let tree = self.cas.capture_dir_filtered(&canonical, &excludes)?;
                    mounts.push((mount_path, tree));
                }
            } else {
                let parent = relative.parent().unwrap_or(Path::new(""));
                let name = relative
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| PlanError::Message(format!("invalid {what} path")))?;
                let blob = self.cas.put_file(&canonical)?;
                let tree = Tree::new(
                    [(
                        name.to_owned(),
                        TreeEntry::File {
                            digest: blob,
                            executable: false,
                        },
                    )]
                    .into_iter()
                    .collect(),
                )
                .map_err(|err| PlanError::Message(format!("invalid tree: {err}")))?;
                let tree = self.cas.put_tree(&tree)?;
                let mount_path = if parent.as_os_str().is_empty() {
                    RelativePath::new(".").unwrap()
                } else {
                    RelativePath::new(&parent.to_string_lossy()).map_err(|err| {
                        PlanError::Message(format!("invalid {what} path {:?}: {err}", path))
                    })?
                };
                mounts.push((mount_path, tree));
            }
        }
        if mounts.is_empty() {
            return Err(PlanError::Message(format!(
                "{what} paths of package {} could not be captured",
                pkg.name
            )));
        }
        self.cas.assemble(&mounts).map_err(|error| {
            PlanError::Message(format!(
                "cannot assemble {what} inputs for {}: {error}",
                pkg.id
            ))
        })
    }

    /// Plans a package's build-script, library, and proc-macro actions.
    fn plan_package_library(
        &mut self,
        actions: &mut Vec<PlannedAction>,
        pkg: &Package,
    ) -> Result<Option<ActionId>, PlanError> {
        let source_tree = self.source_trees[&pkg.id];
        let cc = self.cc_for(&pkg.id);

        // Build script: compile, then run. The run's source tree is
        // narrowed by the previous run's `rerun-if-changed` directives
        // (whole package tree when none were emitted — Cargo's fallback).
        let mut target_bs_run: Option<ActionId> = None;
        let mut host_bs_run: Option<ActionId> = None;
        if let Some(script) = &pkg.build_script {
            let domains: Vec<bool> = [
                self.target_packages.contains(&pkg.id).then_some(false),
                self.host_packages.contains(&pkg.id).then_some(true),
            ]
            .into_iter()
            .flatten()
            .collect();
            for feature_host_domain in domains {
                let suffix = if feature_host_domain { ":host" } else { "" };
                let mut run_source_tree = source_tree;
                let mut run_env: BTreeMap<String, String> = BTreeMap::new();
                // The previous run's directives narrow BOTH the script's
                // compile and run inputs: the run's executable comes from the
                // compile, so a recompile (e.g. triggered by an undeclared
                // file) would produce a new binary and rerun the script,
                // defeating rerun-if-changed.
                let previous = self.previous_directives(pkg, feature_host_domain);
                // The COMPILE input narrows by the previous script-compile's
                // dep-info (rustc's own module closure — `mod rustc;` must
                // stay available); the RUN input narrows by rerun-if-changed.
                let mut script_tree = source_tree;
                let compile_id = format!("rust:bs-compile:{}{suffix}", self.pkg_label(pkg));
                if let Some(paths) = self.previous_dep_info(&compile_id, pkg) {
                    script_tree = self.mount_package_paths(pkg, paths, "dep-info")?;
                }
                if let Some(directives) = &previous {
                    run_source_tree =
                        self.narrowed_script_tree(pkg, run_source_tree, directives)?;
                    // rerun-if-env-changed: declared env vars become explicit
                    // run-action inputs (the digest then covers their values).
                    // An unset var stays absent — forcing it to "" would change
                    // what the script observes (a script's `unwrap_or(default)`
                    // fallback must keep working on rebuilds).
                    for var in &directives.rerun_if_env_changed {
                        if let Ok(value) = std::env::var(var) {
                            run_env.insert(var.clone(), value);
                        }
                    }
                }
                let script = self.crate_root_for(&pkg.id, script);
                let binary = format!("{}_build_script", crate_name(&pkg.name));
                let compile_id = self.plan_compile(
                    actions,
                    &format!("bs-compile:{}{suffix}", self.pkg_key(pkg)),
                    &format!("rust:bs-compile:{}{suffix}", self.pkg_label(pkg)),
                    "RustBuildScriptCompile",
                    pkg,
                    script_tree,
                    cc.clone(),
                    binary.clone(),
                    "bin",
                    None,
                    &pkg.build_deps,
                    None,
                    script.clone(),
                    true,
                    feature_host_domain,
                    false,
                )?;

                if previous.is_some() {
                    run_source_tree = script_tree;
                }

                let run_id = ActionId(format!("rust:bs-run:{}{suffix}", self.pkg_label(pkg)));
                // Direct dependencies with a `links` value export their build
                // metadata to this script as `DEP_<LINKS>_<KEY>`.
                let mut dep_links: Vec<(String, ActionId)> = Vec::new();
                let linked_deps = pkg
                    .deps
                    .iter()
                    .map(|dep| (dep, feature_host_domain))
                    .chain(pkg.build_deps.iter().map(|dep| (dep, true)));
                for (dep, dep_host_domain) in linked_deps {
                    if !self.dep_active(&pkg.id, dep, dep_host_domain) {
                        continue;
                    }
                    let Some(dep_pkg) = self.model.packages.iter().find(|p| p.id == dep.package)
                    else {
                        continue;
                    };
                    let Some(links) = &dep_pkg.links else {
                        continue;
                    };
                    let dep_suffix = if dep_host_domain { ":host" } else { "" };
                    let bs_id = self
                        .planned_ids
                        .get(&format!("bs-run:{}{dep_suffix}", self.pkg_key(dep_pkg)));
                    if let Some(bs_id) = bs_id {
                        dep_links.push((links.clone(), bs_id.clone()));
                    }
                }
                dep_links.sort_by(|a, b| a.0.cmp(&b.0));
                let run_ctx = Ctx {
                    logical_id: run_id.clone(),
                    mnemonic: "RustBuildScriptRun".to_owned(),
                    external: self.pkg_external(pkg),
                    kind: CtxKind::BuildScriptRun(BuildScriptRunSpec {
                        compile: compile_id.clone(),
                        dep_links,
                        binary: binary.clone(),
                        pkg_name: pkg.name.clone(),
                        pkg_version: pkg.version.clone(),
                        host_triple: self.toolchain.host_triple.clone(),
                        target_triple: if feature_host_domain {
                            self.toolchain.host_triple.clone()
                        } else {
                            self.target_triple
                                .clone()
                                .unwrap_or_else(|| self.toolchain.host_triple.clone())
                        },
                        opt_level: self.effective_profile(&pkg.name).opt_level.clone(),
                        debug: self.effective_profile(&pkg.name).debug,
                        rustc_path: self.toolchain.rustc.clone(),
                        rustdoc_path: self
                            .toolchain
                            .rustdoc
                            .clone()
                            .unwrap_or_else(|| self.toolchain.rustc.with_file_name("rustdoc")),
                        encoded_rustflags: self
                            .model
                            .global_rustflags
                            .iter()
                            .chain(pkg.rustflags.iter())
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\u{1f}"),
                        links: pkg.links.clone(),
                        profile: self.profile_name.clone(),
                        cfgs: build_script_cfgs(if feature_host_domain {
                            &self.toolchain.host_triple
                        } else {
                            self.target_triple
                                .as_deref()
                                .unwrap_or(&self.toolchain.host_triple)
                        }),
                        features: self
                            .model
                            .feature_map
                            .features_for(&pkg.id, feature_host_domain)
                            .iter()
                            .cloned()
                            .collect(),
                    }),
                    source_tree: run_source_tree,
                    rustc: self.toolchain.rustc_blob,
                    bundle: Some(self.toolchain.bundle_ref()),
                    properties: self.base_properties(),
                    global_env: self.model.global_env.clone(),
                    pkg_env: {
                        let mut env = self.pkg_cargo_env(pkg);
                        if self.is_member(pkg) {
                            env.insert("CARGO_PRIMARY_PACKAGE".to_owned(), "1".to_owned());
                        }
                        env.extend(run_env);
                        env
                    },
                    cc: Vec::new(),
                    profile_flags: self.profile_flags(pkg),
                    network_allow: self.network_allow,
                    target_triple: if feature_host_domain {
                        Some(self.toolchain.host_triple.clone())
                    } else {
                        self.target_triple.clone()
                    },
                };
                self.planned_ids.insert(
                    format!("bs-run:{}{suffix}", self.pkg_key(pkg)),
                    run_id.clone(),
                );
                actions.push(self.boxed(run_ctx));

                if feature_host_domain {
                    host_bs_run = Some(run_id);
                } else {
                    target_bs_run = Some(run_id);
                }
            }
        }

        // Library / proc-macro actions.
        if let Some(lib) = &pkg.lib {
            let lib_name = lib_crate_name(pkg);
            if lib.proc_macro {
                self.plan_compile(
                    actions,
                    &self.lib_key(pkg, "proc-macro", true),
                    &format!("rust:proc-macro:{}", self.pkg_label(pkg)),
                    "RustProcMacro",
                    pkg,
                    source_tree,
                    cc.clone(),
                    lib_name,
                    "proc-macro",
                    None,
                    &pkg.deps,
                    host_bs_run.clone(),
                    self.crate_root_for(&pkg.id, &lib.path),
                    true,
                    true,
                    true,
                )?;
            } else {
                let types: Vec<CrateType> = if lib.crate_types.is_empty() {
                    vec![CrateType::Rlib]
                } else {
                    lib.crate_types.clone()
                };
                let lib_root = self.crate_root_for(&pkg.id, &lib.path);
                for crate_type in types {
                    if self.target_packages.contains(&pkg.id) {
                        self.plan_compile(
                            actions,
                            &self.lib_key(pkg, crate_type.to_rustc(), false),
                            &format!("rust:lib:{}:{}", self.pkg_label(pkg), crate_type.to_rustc()),
                            "RustLibrary",
                            pkg,
                            source_tree,
                            cc.clone(),
                            lib_name.clone(),
                            crate_type.to_rustc(),
                            None,
                            &pkg.deps,
                            target_bs_run.clone(),
                            lib_root.clone(),
                            false,
                            false,
                            true,
                        )?;
                    }
                    if self.host_packages.contains(&pkg.id) {
                        self.plan_compile(
                            actions,
                            &self.lib_key(pkg, crate_type.to_rustc(), true),
                            &format!(
                                "rust:lib:{}:{}:host",
                                self.pkg_label(pkg),
                                crate_type.to_rustc()
                            ),
                            "RustLibrary",
                            pkg,
                            source_tree,
                            cc.clone(),
                            lib_name.clone(),
                            crate_type.to_rustc(),
                            None,
                            &pkg.deps,
                            host_bs_run.clone(),
                            lib_root.clone(),
                            true,
                            true,
                            true,
                        )?;
                    }
                }
            }
        }

        Ok(target_bs_run.or(host_bs_run))
    }

    /// Plans a package's binary actions; each depends on the package's own
    /// library (when present) plus declared deps.
    fn plan_package_bins(
        &mut self,
        actions: &mut Vec<PlannedAction>,
        pkg: &Package,
    ) -> Result<(), PlanError> {
        let source_tree = self.source_trees[&pkg.id];
        let cc = self.cc_for(&pkg.id);
        let bs_run: Option<ActionId> = self
            .planned_ids
            .get(&format!("bs-run:{}", self.pkg_key(pkg)))
            .cloned();

        if !self.is_selected_member(pkg) {
            return Ok(());
        }
        for bin in &pkg.bins {
            if !self.target_selected("bin", &bin.name) {
                continue;
            }
            if !self.required_features_active(pkg, &bin.required_features) {
                continue;
            }
            let mut deps = pkg.deps.clone();
            if pkg.lib.is_some() {
                deps.insert(
                    0,
                    Dep {
                        extern_name: lib_crate_name(pkg),
                        package: pkg.id.clone(),
                        optional: false,
                        default_features: true,
                        features: Vec::new(),
                        target: None,
                    },
                );
            }
            self.plan_compile(
                actions,
                &format!("bin:{}:{}", self.pkg_key(pkg), bin.name),
                &format!("rust:bin:{}:{}", self.pkg_label(pkg), bin.name),
                "RustBinary",
                pkg,
                source_tree,
                cc.clone(),
                bin.crate_name.clone(),
                "bin",
                Some(bin.name.clone()),
                &deps,
                bs_run.clone(),
                self.crate_root_for(&pkg.id, &bin.path),
                false,
                false,
                true,
            )?;
        }
        Ok(())
    }

    /// Plans a package's example compiles (built in test/`--all-targets`
    /// builds, like cargo; `tong build` skips them). Examples link the
    /// package's library, its deps, and its dev-deps.
    fn plan_package_examples(
        &mut self,
        actions: &mut Vec<PlannedAction>,
        pkg: &Package,
    ) -> Result<(), PlanError> {
        let source_tree = self.source_trees[&pkg.id];
        let cc = self.cc_for(&pkg.id);
        let bs_run: Option<ActionId> = self
            .planned_ids
            .get(&format!("bs-run:{}", self.pkg_key(pkg)))
            .cloned();
        if !self.is_selected_member(pkg) {
            return Ok(());
        }
        for example in &pkg.examples {
            if !self.target_selected("example", &example.name) {
                continue;
            }
            if !self.required_features_active(pkg, &example.required_features) {
                continue;
            }
            let mut deps = pkg.deps.clone();
            deps.extend(pkg.dev_deps.iter().cloned());
            if pkg.lib.is_some() {
                deps.insert(
                    0,
                    Dep {
                        extern_name: lib_crate_name(pkg),
                        package: pkg.id.clone(),
                        optional: false,
                        default_features: true,
                        features: Vec::new(),
                        target: None,
                    },
                );
            }
            let crate_types: Vec<&str> = if example.crate_types.is_empty() {
                vec!["bin"]
            } else {
                example.crate_types.iter().map(String::as_str).collect()
            };
            for crate_type in crate_types {
                let suffix = if example.crate_types.len() > 1 {
                    format!(":{crate_type}")
                } else {
                    String::new()
                };
                self.plan_compile(
                    actions,
                    &format!("example:{}:{}{suffix}", self.pkg_key(pkg), example.name),
                    &format!(
                        "rust:example:{}:{}{suffix}",
                        self.pkg_label(pkg),
                        example.name
                    ),
                    "RustExample",
                    pkg,
                    source_tree,
                    cc.clone(),
                    example.crate_name.clone(),
                    crate_type,
                    (crate_type == "bin").then(|| example.name.clone()),
                    &deps,
                    bs_run.clone(),
                    self.crate_root_for(&pkg.id, &example.path),
                    false,
                    false,
                    true,
                )?;
            }
        }
        Ok(())
    }

    /// Captured source-tree digests of every package (the inputs of the
    /// planned graph). `tong build --deps-only` records them in the
    /// build-state manifest so GC keeps the local packages' manifest-only
    /// trees — the deps stage captures them and the app stage re-captures
    /// identical digests (docs/docker-caching.md).
    pub fn captured_source_trees(&self) -> Vec<tong_core::digest::Digest> {
        self.source_trees
            .values()
            .map(|tree| tree.digest())
            .collect()
    }

    /// Final runnable artifacts (binaries) with their runtime closures.
    pub fn final_artifacts(&self) -> Vec<FinalArtifact> {
        let mut out = Vec::new();
        for pkg in &self.model.packages {
            if !self.is_selected_member(pkg) {
                continue;
            }
            let runtime: Vec<(BlobDigest, String)> = self
                .cc_closure
                .get(&pkg.id)
                .unwrap()
                .iter()
                .map(|name| {
                    let info = &self.cc[name];
                    (info.blob, info.file_name.clone())
                })
                .collect();
            for bin in &pkg.bins {
                if !self.target_selected("bin", &bin.name) {
                    continue;
                }
                let Some(action) =
                    self.planned_ids
                        .get(&format!("bin:{}:{}", self.pkg_key(pkg), bin.name))
                else {
                    continue;
                };
                let Ok(output) = OutputPath::new(&bin.name) else {
                    continue;
                };
                out.push(FinalArtifact {
                    name: bin.name.clone(),
                    action: action.clone(),
                    output,
                    runtime: runtime.clone(),
                });
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_compile(
        &mut self,
        actions: &mut Vec<PlannedAction>,
        key: &str,
        logical_id: &str,
        mnemonic: &str,
        pkg: &Package,
        source_tree: TreeDigest,
        cc: Vec<(String, TreeDigest, String)>,
        crate_name: String,
        crate_type: &str,
        output_name: Option<String>,
        deps: &[Dep],
        build_script: Option<ActionId>,
        crate_root: PathBuf,
        host_unit: bool,
        feature_host_domain: bool,
        own_directives: bool,
    ) -> Result<ActionId, PlanError> {
        let mut source_tree = source_tree;
        // Narrow the input tree to the files the previous build's rustc
        // actually read (dep-info). First builds stay conservative; a
        // malformed or out-of-package dep-info also keeps the whole tree
        // (never cache under an incomplete key).
        if let Some(paths) = self.previous_dep_info(logical_id, pkg) {
            source_tree = self.mount_package_paths(pkg, paths, "dep-info")?;
        }
        // `--check` builds emit metadata only (cargo check); build
        // scripts always compile fully (they run even in check builds).
        let proc_macro_test =
            mnemonic == "RustTestCompile" && pkg.lib.as_ref().is_some_and(|lib| lib.proc_macro);
        let force_full_codegen = (mnemonic == "RustLibrary"
            && self.full_codegen_packages.contains(&pkg.id))
            || proc_macro_test;
        let check = self.check && !host_unit && !force_full_codegen;
        let mut profile_flags = self.profile_flags(pkg);
        // LTO is not supported for proc-macro crate types; Cargo disables it
        // automatically.
        if crate_type == "proc-macro" {
            let mut index = 0;
            while index < profile_flags.len() {
                if profile_flags[index] == "-C"
                    && profile_flags
                        .get(index + 1)
                        .is_some_and(|flag| flag.starts_with("lto="))
                {
                    profile_flags.drain(index..index + 2);
                } else {
                    index += 1;
                }
            }
        }
        let target_name = match mnemonic {
            "RustBuildScriptCompile" => "build-script".to_owned(),
            "RustProcMacro" | "RustLibrary" => "lib".to_owned(),
            "RustBinary" => format!("bin:{}", output_name.as_deref().unwrap_or(&crate_name)),
            "RustExample" => {
                format!("example:{}", output_name.as_deref().unwrap_or(&crate_name))
            }
            "RustTestCompile" => {
                format!("test:{}", output_name.as_deref().unwrap_or(&crate_name))
            }
            _ => key.to_owned(),
        };
        let unit = self.unit_id(
            pkg,
            target_name,
            crate_type,
            host_unit,
            feature_host_domain,
            check,
            profile_flags.clone(),
        );
        let meta = unit.artifact_hash();
        let check_output = |output: String| {
            if check {
                format!("{output}.rmeta")
            } else {
                output
            }
        };
        let output = if let Some(name) = output_name {
            check_output(name)
        } else if crate_type == "bin" {
            check_output(crate_name.clone())
        } else {
            // Check builds produce `.rmeta` (consumers reference the same
            // name).
            let ext = if check && crate_type != "proc-macro" {
                "rmeta"
            } else if crate_type == "proc-macro" {
                dll_extension()
            } else {
                match crate_type {
                    "lib" | "rlib" => "rlib",
                    "staticlib" => "a",
                    _ => dll_extension(),
                }
            };
            format!("lib{crate_name}-{meta}.{ext}")
        };
        let dep_specs = self.resolve_deps(&pkg.id, deps, host_unit)?;
        // Transitive closure of the direct deps: rustc resolves transitive
        // rlibs through `-L dependency=...`, so every reachable crate's
        // output tree must be mounted at `deps/` (Cargo puts all rlibs in
        // one directory). Scheduling waits for all of them.
        let transitive_deps = self.transitive_dep_specs(&pkg.id, deps, host_unit)?;
        let bin_env = if mnemonic == "RustTestCompile" {
            self.package_bin_env(pkg)
        } else {
            Vec::new()
        };
        // Per-crate feature cfgs: `--cfg feature="<name>"` for every
        // activated feature (sorted), mirroring Cargo.
        let feature_cfgs: Vec<String> = unit
            .features
            .iter()
            .flat_map(|feature| ["--cfg".to_owned(), format!("feature=\"{feature}\"")])
            .collect();
        let mut extra_flags: Vec<String> = self
            .model
            .global_rustflags
            .iter()
            .chain(pkg.rustflags.iter())
            .cloned()
            .collect();
        extra_flags.extend(["--check-cfg".to_owned(), "cfg(docsrs,test)".to_owned()]);
        let declared_features = pkg
            .features
            .keys()
            .map(|feature| format!("\"{feature}\""))
            .collect::<Vec<_>>()
            .join(",");
        extra_flags.extend([
            "--check-cfg".to_owned(),
            format!("cfg(feature,values({declared_features}))"),
        ]);
        // Cargo passes `--cap-lints allow` to registry dependencies (lints
        // of external crates are the maintainers' concern, and deny-by-
        // default lints in newer rustc would break old crates like mime).
        // Workspace packages keep their lints.
        if self.pkg_external(pkg) {
            extra_flags.push("--cap-lints".to_owned());
            extra_flags.push("allow".to_owned());
        }
        let ctx = Ctx {
            logical_id: ActionId(logical_id.to_owned()),
            mnemonic: mnemonic.to_owned(),
            external: self.pkg_external(pkg),
            kind: CtxKind::Compile(CompileSpec {
                crate_name,
                edition: pkg.edition,
                crate_type: crate_type.to_owned(),
                meta,
                output,
                deps: dep_specs,
                build_script,
                directive_sources: self.link_directive_sources(
                    pkg,
                    own_directives,
                    feature_host_domain,
                ),
                crate_root,
                extra_flags,
                feature_cfgs,
                transitive_deps,
                is_proc_macro: pkg.lib.as_ref().is_some_and(|lib| lib.proc_macro),
                host_unit,
                primary: !self.pkg_external(pkg),
                check,
                bin_env,
            }),
            source_tree,
            rustc: self.toolchain.rustc_blob,
            bundle: Some(self.toolchain.bundle_ref()),
            properties: self.base_properties(),
            global_env: self.model.global_env.clone(),
            pkg_env: self.pkg_cargo_env(pkg),
            cc,
            profile_flags: profile_flags.clone(),
            network_allow: self.network_allow,
            target_triple: self.target_triple.clone(),
        };
        let id = ctx.logical_id.clone();
        self.planned_ids.insert(key.to_owned(), id.clone());
        actions.push(self.boxed(ctx));
        Ok(id)
    }

    /// Whether every feature in `required` is active on the package
    /// (Cargo `required-features`: targets with unmet requirements are not
    /// built).
    fn required_features_active(&self, pkg: &Package, required: &[String]) -> bool {
        if required.is_empty() {
            return true;
        }
        let active = self.model.feature_map.features_for(&pkg.id, false);
        required.iter().all(|feature| active.contains(feature))
    }

    fn package_bin_env(&self, pkg: &Package) -> Vec<(String, ActionId, String)> {
        pkg.bins
            .iter()
            .filter(|bin| self.required_features_active(pkg, &bin.required_features))
            .filter_map(|bin| {
                self.planned_ids
                    .get(&format!("bin:{}:{}", self.pkg_key(pkg), bin.name))
                    .cloned()
                    .map(|action| (bin.name.clone(), action, bin.name.clone()))
            })
            .collect()
    }

    /// Whether a package belongs to the workspace (`model.members`);
    /// everything else — registry, git, and path-outside-workspace
    /// dependencies — is external. `tong build --deps-only` executes only
    /// external actions.
    fn pkg_external(&self, pkg: &Package) -> bool {
        !self.model.members.contains(&pkg.id)
    }

    /// Internal disambiguated key for a package's planned actions:
    /// name@version@source. Never surfaces in digests or output.
    /// Whether `pkg` is a workspace member. Cargo builds dependency
    /// packages as libraries only — test/bench/example/bin targets of
    /// non-members are never planned (`--all-targets` covers members).
    fn is_member(&self, pkg: &Package) -> bool {
        self.model.members.contains(&pkg.id)
    }

    fn is_selected_member(&self, pkg: &Package) -> bool {
        self.is_member(pkg)
            && (self.model.configured_members.is_empty()
                || self.model.configured_members.contains(&pkg.id))
    }

    fn target_selected(&self, kind: &str, name: &str) -> bool {
        self.model.configured_targets.is_empty()
            || self
                .model
                .configured_targets
                .iter()
                .any(|(selected_kind, selected_name)| {
                    selected_kind == kind && selected_name == name
                })
    }

    /// The profile for `package_name`: an exact or glob-matching
    /// `[profile.<name>.package.<spec>]` override, else the active
    /// profile.
    fn effective_profile(&self, package_name: &str) -> &ProfileSpec {
        if let Some(profile) = self.package_profiles.get(package_name) {
            return profile;
        }
        for (spec, profile) in &self.package_profiles {
            if glob_match(spec, package_name) {
                return profile;
            }
        }
        &self.profile
    }

    fn profile_flags(&self, pkg: &Package) -> Vec<String> {
        let mut flags = self.effective_profile(&pkg.name).rustc_flags();
        if self.tests_enabled {
            // Stable Cargo ignores profile `panic = "abort"` for the
            // complete test graph. Every linked dependency must use the
            // same unwind strategy as the harness.
            for index in 0..flags.len().saturating_sub(1) {
                if flags[index] == "-C" && flags[index + 1].starts_with("panic=") {
                    flags[index + 1] = "panic=unwind".to_owned();
                }
            }
        }
        flags
    }

    fn pkg_key(&self, pkg: &Package) -> String {
        format!("{}@{}@{}", pkg.name, pkg.version, pkg.id.lock_source())
    }

    fn lib_key(&self, pkg: &Package, crate_type: &str, host: bool) -> String {
        format!(
            "lib:{}:{crate_type}:{}",
            self.pkg_key(pkg),
            if host { "host" } else { "target" }
        )
    }

    /// Human label for a package in logical ids: the bare name when the
    /// workspace has one package with that name, `name@version` when only
    /// versions differ, and a source-aware suffix for otherwise identical
    /// names and versions.
    fn pkg_label(&self, pkg: &Package) -> String {
        let same_name = self
            .model
            .packages
            .iter()
            .filter(|other| other.name == pkg.name)
            .count();
        if same_name == 1 {
            return pkg.name.clone();
        }

        let same_version = self
            .model
            .packages
            .iter()
            .filter(|other| other.name == pkg.name && other.version == pkg.version)
            .count();
        if same_version == 1 {
            format!("{}@{}", pkg.name, pkg.version)
        } else {
            format!("{}@{}#{}", pkg.name, pkg.version, pkg.id.identity_hash())
        }
    }

    /// Cargo package variables are always defined; absent manifest values
    /// become empty strings so `env!` behaves exactly as under Cargo.
    fn pkg_cargo_env(&self, pkg: &Package) -> BTreeMap<String, String> {
        let mut env = pkg.env.clone();
        env.insert("CARGO_PKG_NAME".to_owned(), pkg.name.clone());
        env.insert("CARGO_PKG_VERSION".to_owned(), pkg.version.clone());
        env.insert(
            "CARGO_PKG_AUTHORS".to_owned(),
            pkg.metadata.authors.join(":"),
        );
        for (key, value) in [
            ("CARGO_PKG_DESCRIPTION", pkg.metadata.description.as_deref()),
            ("CARGO_PKG_HOMEPAGE", pkg.metadata.homepage.as_deref()),
            ("CARGO_PKG_REPOSITORY", pkg.metadata.repository.as_deref()),
            ("CARGO_PKG_LICENSE", pkg.metadata.license.as_deref()),
            (
                "CARGO_PKG_RUST_VERSION",
                pkg.metadata.rust_version.as_deref(),
            ),
            ("CARGO_PKG_README", pkg.metadata.readme.as_deref()),
        ] {
            env.insert(key.to_owned(), value.unwrap_or_default().to_owned());
        }
        env.insert(
            "CARGO_PKG_LICENSE_FILE".to_owned(),
            pkg.metadata
                .license_file
                .as_deref()
                .map(|path| format!("{EXEC_ROOT_VAR}/in/{path}"))
                .unwrap_or_default(),
        );
        if let Ok(version) = semver::Version::parse(&pkg.version) {
            env.extend([
                (
                    "CARGO_PKG_VERSION_MAJOR".to_owned(),
                    version.major.to_string(),
                ),
                (
                    "CARGO_PKG_VERSION_MINOR".to_owned(),
                    version.minor.to_string(),
                ),
                (
                    "CARGO_PKG_VERSION_PATCH".to_owned(),
                    version.patch.to_string(),
                ),
                ("CARGO_PKG_VERSION_PRE".to_owned(), version.pre.to_string()),
            ]);
        }
        env
    }

    fn boxed(&self, ctx: Ctx) -> PlannedAction {
        let logical_id = ctx.logical_id.clone();
        let mnemonic = ctx.mnemonic.clone();
        let external = ctx.external;
        PlannedAction {
            logical_id,
            mnemonic,
            external,
            deps: ctx.deps(),
            make: Box::new(move |completed, cas| concretize(&ctx, completed, cas)),
        }
    }

    fn base_properties(&self) -> BTreeMap<String, CanonicalValue> {
        BTreeMap::from([
            (
                "tong.rust.profile".to_owned(),
                CanonicalValue::String(self.profile_name.clone()),
            ),
            (
                "tong.rust.rustc_verbose_version".to_owned(),
                CanonicalValue::String(self.toolchain.version_verbose.clone()),
            ),
            (
                "tong.rust.sysroot_tree".to_owned(),
                CanonicalValue::String(self.toolchain.sysroot_tree.digest().to_hex()),
            ),
            (
                "tong.execution.portable".to_owned(),
                CanonicalValue::String("false".to_owned()),
            ),
        ])
    }

    #[allow(clippy::too_many_arguments)]
    fn unit_id(
        &self,
        pkg: &Package,
        target: String,
        crate_type: &str,
        host_unit: bool,
        feature_host_domain: bool,
        check: bool,
        profile_flags: Vec<String>,
    ) -> RustUnitId {
        RustUnitId {
            package: pkg.id.clone(),
            target,
            crate_type: crate_type.to_owned(),
            domain: if host_unit {
                RustUnitDomain::Host
            } else {
                RustUnitDomain::Target
            },
            target_triple: if host_unit {
                self.toolchain.host_triple.clone()
            } else {
                self.target_triple
                    .clone()
                    .unwrap_or_else(|| self.toolchain.host_triple.clone())
            },
            profile: self.profile_name.clone(),
            profile_flags,
            features: self
                .model
                .feature_map
                .features_for(&pkg.id, feature_host_domain)
                .clone(),
            mode: if check {
                RustUnitMode::Check
            } else {
                RustUnitMode::Build
            },
        }
    }

    /// The compile-time crate root for a package-relative path, rewritten
    /// when the file lives outside the package dir (mounted at `ext/<n>`).
    fn crate_root_for(&self, pkg: &PackageId, original: &Path) -> PathBuf {
        self.crate_roots
            .get(&(pkg.clone(), original.to_path_buf()))
            .cloned()
            .unwrap_or_else(|| original.to_path_buf())
    }

    /// Build-script run ids whose link directives apply to `pkg`'s crates:
    /// the package's own script plus the scripts of every transitive
    /// dependency (Cargo propagates link directives to all dependents).
    /// Inactive optional dep edges are skipped.
    fn link_directive_sources(
        &self,
        pkg: &Package,
        include_own: bool,
        host_domain: bool,
    ) -> Vec<ActionId> {
        fn active_deps<'b>(model: &RustModel, pkg: &'b Package, host_domain: bool) -> Vec<&'b Dep> {
            pkg.deps
                .iter()
                .chain(pkg.build_deps.iter())
                .filter(|dep| {
                    if !dep.optional {
                        return true;
                    }
                    model
                        .feature_map
                        .edge_active_for(&pkg.id, &dep.extern_name, host_domain)
                })
                .collect()
        }

        let mut out = Vec::new();
        let suffix = if host_domain { ":host" } else { "" };
        // The package's own build-script run applies to its lib/test
        // compiles; a build-script COMPILE must not depend on its own run
        // (that would be a self-cycle).
        if include_own
            && let Some(id) = self
                .planned_ids
                .get(&format!("bs-run:{}{suffix}", self.pkg_key(pkg)))
        {
            out.push(id.clone());
        }
        let mut seen: BTreeSet<PackageId> = BTreeSet::new();
        let mut stack: Vec<PackageId> = active_deps(self.model, pkg, host_domain)
            .iter()
            .map(|dep| dep.package.clone())
            .collect();
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(dep) = self.model.packages.iter().find(|p| p.id == id) {
                if let Some(bs) = self
                    .planned_ids
                    .get(&format!("bs-run:{}{suffix}", self.pkg_key(dep)))
                {
                    out.push(bs.clone());
                }
                stack.extend(
                    active_deps(self.model, dep, host_domain)
                        .iter()
                        .map(|d| d.package.clone()),
                );
            }
        }
        out
    }

    fn resolve_deps(
        &self,
        pkg: &PackageId,
        deps: &[Dep],
        host: bool,
    ) -> Result<Vec<DepSpec>, PlanError> {
        deps.iter()
            .filter(|dep| self.dep_active(pkg, dep, host))
            .map(|dep| self.dep_spec(dep, host))
            .collect()
    }

    /// The transitive closure of a dep list: every Rust crate action
    /// reachable through the dependency graph (feature-gated; native
    /// imports are leaves). Deduplicated by action id.
    fn transitive_dep_specs(
        &self,
        pkg: &PackageId,
        deps: &[Dep],
        host: bool,
    ) -> Result<Vec<DepSpec>, PlanError> {
        let mut out: Vec<DepSpec> = Vec::new();
        let mut seen: BTreeSet<ActionId> = BTreeSet::new();
        let mut frontier: Vec<(&PackageId, &Dep, bool)> =
            deps.iter().map(|dep| (pkg, dep, host)).collect();
        while let Some((parent, dep, domain)) = frontier.pop() {
            if !self.dep_active(parent, dep, domain) {
                continue;
            }
            let spec = self.dep_spec(dep, domain)?;
            if let DepSpec::Rust { action, .. } = &spec {
                if !seen.insert(action.clone()) {
                    continue;
                }
                if let Some(pkg) = self.model.packages.iter().find(|p| p.id == dep.package) {
                    let next_domain = domain || pkg.lib.as_ref().is_some_and(|lib| lib.proc_macro);
                    frontier.extend(pkg.deps.iter().map(|next| (&pkg.id, next, next_domain)));
                }
            }
            out.push(spec);
        }
        // Deterministic order (scheduling + mounts): sort by action id.
        out.sort_by_key(|spec| match spec {
            DepSpec::Rust { action, .. } => action.clone(),
            DepSpec::Native => ActionId(String::new()),
        });
        Ok(out)
    }

    /// Whether a dep edge is live: non-optional edges always; optional
    /// edges only when the feature resolution activated them; target-
    /// specific edges only when they match the unit's effective triple.
    /// The import keeps every target's deps in the resolved graph, so the
    /// configured unit graph filters them here.
    fn dep_active(&self, pkg: &PackageId, dep: &Dep, host_domain: bool) -> bool {
        let dependency_is_proc_macro = self
            .model
            .packages
            .iter()
            .find(|package| package.id == dep.package)
            .and_then(|package| package.lib.as_ref())
            .is_some_and(|lib| lib.proc_macro);
        let edge_host_domain = host_domain || dependency_is_proc_macro;
        if let Some(target) = &dep.target {
            let triple = if edge_host_domain {
                &self.toolchain.host_triple
            } else {
                self.target_triple
                    .as_ref()
                    .unwrap_or(&self.toolchain.host_triple)
            };
            if !crate::cargo_import::target_matches(target, triple, "dependency").unwrap_or(false) {
                return false;
            }
        }
        if !dep.optional {
            return true;
        }
        self.model
            .feature_map
            .edge_active_for(pkg, &dep.extern_name, edge_host_domain)
    }

    /// Resolves a dependency to its producer action or native import.
    /// An unresolvable dependency is a plan error — silently dropping it
    /// would link against a missing crate and fail deep inside rustc.
    fn dep_spec(&self, dep: &Dep, host_domain: bool) -> Result<DepSpec, PlanError> {
        if self.cc.contains_key(&dep.package.name) {
            return Ok(DepSpec::Native);
        }
        let pkg = self
            .model
            .packages
            .iter()
            .find(|p| p.id == dep.package)
            .ok_or_else(|| {
                PlanError::Message(format!(
                    "dependency {:?} of {:?} names no imported package or cc_import; \
                     if it is a registry dependency missing from Tong.lock, run \
                     `tong lock` (the lockfile may be stale for the requested features)",
                    dep.extern_name, dep.package.name
                ))
            })?;
        let (kind, ext, key) = if let Some(lib) = &pkg.lib {
            if lib.proc_macro {
                (
                    "proc-macro",
                    dll_extension(),
                    self.lib_key(pkg, "proc-macro", true),
                )
            } else {
                let types: Vec<CrateType> = if lib.crate_types.is_empty() {
                    vec![CrateType::Rlib]
                } else {
                    lib.crate_types.clone()
                };
                let chosen = *types
                    .iter()
                    .find(|t| **t == CrateType::Rlib)
                    .or_else(|| types.iter().find(|t| **t == CrateType::Staticlib))
                    .or_else(|| types.iter().find(|t| **t == CrateType::Cdylib))
                    .or_else(|| types.iter().find(|t| **t == CrateType::Dylib))
                    .ok_or_else(|| {
                        PlanError::Message(format!(
                            "cannot link dependency {:?}: no linkable crate type",
                            pkg.name
                        ))
                    })?;
                let ext = match chosen {
                    CrateType::Rlib => "rlib",
                    CrateType::Staticlib => "a",
                    _ => dll_extension(),
                };
                (
                    chosen.to_rustc(),
                    ext,
                    self.lib_key(pkg, chosen.to_rustc(), host_domain),
                )
            }
        } else {
            return Err(PlanError::Message(format!(
                "dependency {:?} names package {:?}, which has no library target",
                dep.extern_name, dep.package.name
            )));
        };
        let action = self
            .planned_ids
            .get(&key)
            .cloned()
            .ok_or_else(|| PlanError::Message(format!("no planned action for {key}")))?;
        let lib_name = lib_crate_name(pkg);
        let host_unit = kind == "proc-macro" || host_domain;
        let checked = self.check && !host_unit && !self.full_codegen_packages.contains(&pkg.id);
        let mut profile_flags = self.profile_flags(pkg);
        if kind == "proc-macro" {
            let mut index = 0;
            while index < profile_flags.len() {
                if profile_flags[index] == "-C"
                    && profile_flags
                        .get(index + 1)
                        .is_some_and(|flag| flag.starts_with("lto="))
                {
                    profile_flags.drain(index..index + 2);
                } else {
                    index += 1;
                }
            }
        }
        let meta = self
            .unit_id(
                pkg,
                "lib".to_owned(),
                kind,
                host_unit,
                host_unit,
                checked,
                profile_flags,
            )
            .artifact_hash();
        // Check builds produce `.rmeta` instead of `.rlib` (the producer
        // and every consumer must agree on the file name).
        // Check target units produce `.rmeta`; host and proc-macro units
        // are always fully code-generated and therefore expose `.rlib`
        // or a dynamic library to their consumers.
        let ext = if checked && ext == "rlib" {
            "rmeta"
        } else {
            ext
        };
        Ok(DepSpec::Rust {
            extern_name: dep.extern_name.clone(),
            action,
            file: format!("lib{lib_name}-{meta}.{ext}"),
        })
    }

    /// Transitive native closure of a package: every `cc_import` reachable
    /// through the dependency graph (inactive optional edges skipped).
    fn collect_cc(&self, pkg: &Package, pkg_map: &BTreeMap<PackageId, &Package>) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut stack = vec![pkg.id.clone()];
        while let Some(id) = stack.pop() {
            let Some(current) = pkg_map.get(&id) else {
                continue;
            };
            for dep in current
                .deps
                .iter()
                .chain(current.build_deps.iter())
                .filter(|dep| {
                    if !dep.optional {
                        return true;
                    }
                    self.model
                        .feature_map
                        .edge_active(&current.id, &dep.extern_name)
                })
            {
                if self.cc.contains_key(&dep.package.name) {
                    if seen.insert(dep.package.clone()) {
                        out.push(dep.package.name.clone());
                    }
                } else if pkg_map.contains_key(&dep.package) {
                    stack.push(dep.package.clone());
                }
            }
        }
        out.sort();
        out
    }

    fn cc_for(&self, pkg: &PackageId) -> Vec<(String, TreeDigest, String)> {
        self.cc_closure
            .get(pkg)
            .unwrap()
            .iter()
            .map(|name| {
                let info = &self.cc[name];
                (name.clone(), info.tree, info.link_name.clone())
            })
            .collect()
    }

    fn import_cc(&mut self, import: &crate::model::CcImport) -> Result<(), PlanError> {
        let blob = self.cas.put_file(&import.shared)?;
        let file_name = import
            .shared
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("lib")
            .to_owned();
        let entries = BTreeMap::from([(
            file_name.clone(),
            TreeEntry::File {
                digest: blob,
                executable: false,
            },
        )]);
        let tree = Tree::new(entries)
            .map_err(|err| PlanError::Message(format!("cc_import {}: {err}", import.name)))?;
        let tree = self.cas.put_tree(&tree)?;
        self.cc.insert(
            import.name.clone(),
            CcInfo {
                tree,
                file_name,
                blob,
                link_name: import.link_name.clone(),
            },
        );
        Ok(())
    }
}

impl Ctx {
    fn deps(&self) -> Vec<ActionId> {
        let mut deps: Vec<ActionId> = match &self.kind {
            CtxKind::Compile(spec) => spec
                .deps
                .iter()
                .chain(spec.transitive_deps.iter())
                .filter_map(|dep| match dep {
                    DepSpec::Rust { action, .. } => Some(action.clone()),
                    DepSpec::Native => None,
                })
                .chain(spec.bin_env.iter().map(|(_, action, _)| action.clone()))
                .chain(spec.build_script.clone())
                .collect(),
            CtxKind::BuildScriptRun(spec) => std::iter::once(spec.compile.clone())
                .chain(spec.dep_links.iter().map(|(_, action)| action.clone()))
                .collect(),
            CtxKind::TestRun(spec) => std::iter::once(spec.compile.clone())
                .filter(|action| !action.0.is_empty())
                .chain(spec.bin_env.iter().map(|(_, action, _)| action.clone()))
                .collect(),
        };
        deps.sort();
        deps.dedup();
        deps
    }

    /// The executable artifact, resolved once dependencies complete (a
    /// build-script binary lives inside its compile action's output tree;
    /// a test binary inside its test-compile action's output tree).
    fn executable(&self, completed: &dyn Completed) -> Result<ArtifactRef, PlanError> {
        match &self.kind {
            CtxKind::Compile(_) => Ok(ArtifactRef::Blob(self.rustc)),
            CtxKind::TestRun(spec) if spec.doc => Ok(ArtifactRef::Blob(self.rustc)),
            CtxKind::BuildScriptRun(spec) => {
                let tree = completed
                    .output_tree(&spec.compile)
                    .ok_or_else(|| PlanError::MissingDependency(spec.compile.clone()))?;
                let path = RelativePath::new(&spec.binary)
                    .map_err(|err| PlanError::Message(err.to_string()))?;
                Ok(ArtifactRef::TreeFile { tree, path })
            }
            CtxKind::TestRun(spec) => {
                let tree = completed
                    .output_tree(&spec.compile)
                    .ok_or_else(|| PlanError::MissingDependency(spec.compile.clone()))?;
                let path = RelativePath::new(&spec.binary)
                    .map_err(|err| PlanError::Message(err.to_string()))?;
                Ok(ArtifactRef::TreeFile { tree, path })
            }
        }
    }
}

/// Configured build-unit package closure. Tong.lock and the imported model
/// retain Cargo's all-target graph, but only active edges reachable from
/// selected workspace roots become actions.
fn configured_package_domains(
    model: &RustModel,
    include_dev: bool,
    target_triple: &str,
    host_triple: &str,
) -> (
    BTreeSet<PackageId>,
    BTreeSet<PackageId>,
    BTreeSet<PackageId>,
) {
    let packages: BTreeMap<&PackageId, &Package> =
        model.packages.iter().map(|pkg| (&pkg.id, pkg)).collect();
    let mut closure: BTreeSet<PackageId> = if model.configured_members.is_empty() {
        model.members.iter().cloned().collect()
    } else {
        model.configured_members.iter().cloned().collect()
    };
    let mut open: Vec<(PackageId, bool)> = closure
        .iter()
        .cloned()
        .map(|package| (package, false))
        .collect();
    // A selected proc-macro package has two configured roles: its macro
    // library is a host unit, while its unit tests are target units. Seed
    // both domains so their dependency closures cannot alias or disappear.
    open.extend(
        closure
            .iter()
            .filter(|id| {
                packages
                    .get(id)
                    .and_then(|package| package.lib.as_ref())
                    .is_some_and(|lib| lib.proc_macro)
            })
            .map(|id| (id.clone(), true)),
    );
    let mut visited = BTreeSet::new();
    let mut target_packages = BTreeSet::new();
    let mut host_packages = BTreeSet::new();
    while let Some((id, host_domain)) = open.pop() {
        if !visited.insert((id.clone(), host_domain)) {
            continue;
        }
        if host_domain {
            host_packages.insert(id.clone());
        } else {
            target_packages.insert(id.clone());
        }
        let Some(package) = packages.get(&id) else {
            continue;
        };
        let normal = package.deps.iter().map(|dep| {
            let proc_macro = packages
                .get(&dep.package)
                .and_then(|package| package.lib.as_ref())
                .is_some_and(|lib| lib.proc_macro);
            (dep, host_domain || proc_macro)
        });
        let build = package.build_deps.iter().map(|dep| (dep, true));
        let dev = include_dev
            .then_some(package.dev_deps.iter().map(|dep| {
                let proc_macro = packages
                    .get(&dep.package)
                    .and_then(|package| package.lib.as_ref())
                    .is_some_and(|lib| lib.proc_macro);
                (dep, proc_macro)
            }))
            .into_iter()
            .flatten();
        for (dep, edge_host_domain) in normal.chain(build).chain(dev) {
            let triple = if edge_host_domain {
                host_triple
            } else {
                target_triple
            };
            if dep.target.as_deref().is_some_and(|target| {
                !crate::cargo_import::target_matches(target, triple, "dependency").unwrap_or(false)
            }) || (dep.optional
                && !model.feature_map.edge_active_for(
                    &package.id,
                    &dep.extern_name,
                    edge_host_domain,
                ))
            {
                continue;
            }
            closure.insert(dep.package.clone());
            open.push((dep.package.clone(), edge_host_domain));
        }
    }
    (closure, target_packages, host_packages)
}

/// Target-domain dependencies that rustc must be able to load as rlibs
/// while checking proc-macro package unit tests. Cargo's check graph keeps
/// this linkable closure distinct from ordinary metadata-only units.
fn proc_macro_test_codegen_closure(model: &RustModel, target_triple: &str) -> BTreeSet<PackageId> {
    let packages: BTreeMap<&PackageId, &Package> =
        model.packages.iter().map(|pkg| (&pkg.id, pkg)).collect();
    let roots = if model.configured_members.is_empty() {
        &model.members
    } else {
        &model.configured_members
    };
    let mut full = BTreeSet::new();
    let mut open: Vec<PackageId> = Vec::new();

    for id in roots {
        let Some(package) = packages.get(id) else {
            continue;
        };
        if !package.lib.as_ref().is_some_and(|lib| lib.proc_macro) {
            continue;
        }
        full.insert(id.clone());
        open.extend(
            package
                .deps
                .iter()
                .chain(package.dev_deps.iter())
                .filter(|dep| configured_target_edge(model, package, dep, target_triple))
                .map(|dep| dep.package.clone()),
        );
    }

    while let Some(id) = open.pop() {
        let Some(package) = packages.get(&id) else {
            continue;
        };
        // Proc macros are already full-codegen host units. Their own
        // dependencies belong to that host closure, not this target one.
        if package.lib.as_ref().is_some_and(|lib| lib.proc_macro) || !full.insert(id.clone()) {
            continue;
        }
        open.extend(
            package
                .deps
                .iter()
                .filter(|dep| configured_target_edge(model, package, dep, target_triple))
                .map(|dep| dep.package.clone()),
        );
    }
    full
}

fn configured_target_edge(
    model: &RustModel,
    package: &Package,
    dep: &Dep,
    target_triple: &str,
) -> bool {
    if dep.target.as_deref().is_some_and(|target| {
        !crate::cargo_import::target_matches(target, target_triple, "dependency").unwrap_or(false)
    }) {
        return false;
    }
    !dep.optional
        || model
            .feature_map
            .edge_active_for(&package.id, &dep.extern_name, false)
}

/// Matches a cargo `[profile.<name>.package.<spec>]` package spec against
/// a package name: exact equality, or a `*` glob (cargo semantics).
fn glob_match(spec: &str, name: &str) -> bool {
    if spec == name {
        return true;
    }
    if let Some((prefix, suffix)) = spec.split_once('*') {
        if prefix.is_empty() && suffix.is_empty() {
            return true;
        }
        if name.starts_with(prefix)
            && name.ends_with(suffix)
            && name.len() >= prefix.len() + suffix.len()
        {
            return true;
        }
    }
    false
}

/// Concretizes a planned action into a full `ActionSpec`.
/// `CARGO_CFG_*` values Cargo exposes for the configured target.
fn build_script_cfgs(target_triple: &str) -> Vec<(String, String)> {
    let facts = tong_core::platform::parse_triple(target_triple).unwrap_or_default();
    let mut out = vec![
        ("target_arch".to_owned(), facts.arch.clone()),
        ("target_os".to_owned(), facts.os.clone()),
        ("target_family".to_owned(), facts.family.clone()),
        ("target_env".to_owned(), facts.env.clone()),
        ("target_vendor".to_owned(), facts.vendor.clone()),
        (
            "target_pointer_width".to_owned(),
            facts.pointer_width.clone(),
        ),
        // Every currently supported Tong target is little-endian. Keep
        // this explicit: real build scripts (for example rustix) require
        // Cargo's `CARGO_CFG_TARGET_ENDIAN` even on common hosts.
        ("target_endian".to_owned(), facts.endian.clone()),
        ("target_abi".to_owned(), String::new()),
    ];
    if facts.family == "unix" {
        out.push(("unix".to_owned(), String::new()));
    }
    if facts.family == "windows" {
        out.push(("windows".to_owned(), String::new()));
    }
    out
}

fn concretize(ctx: &Ctx, completed: &dyn Completed, cas: &Cas) -> Result<ActionSpec, PlanError> {
    let mut mounts: Vec<(RelativePath, TreeDigest)> =
        vec![(RelativePath::new(".").unwrap(), ctx.source_tree)];
    let mut args = Vec::new();
    let mut env = ctx.global_env.clone();
    env.extend(ctx.pkg_env.clone());

    match &ctx.kind {
        CtxKind::BuildScriptRun(spec) => {
            let compile_output = completed
                .output_tree(&spec.compile)
                .ok_or_else(|| PlanError::MissingDependency(spec.compile.clone()))?;
            // The compiled binary must be executable inside the input root
            // so the TreeFile resolution can hardlink it.
            mounts.push((RelativePath::new(".").unwrap(), compile_output));
            env.extend([
                ("OUT_DIR".to_owned(), format!("{EXEC_ROOT_VAR}/out")),
                ("TARGET".to_owned(), spec.target_triple.clone()),
                ("HOST".to_owned(), spec.host_triple.clone()),
                ("OPT_LEVEL".to_owned(), spec.opt_level.clone()),
                ("DEBUG".to_owned(), spec.debug.to_string()),
                ("PROFILE".to_owned(), spec.profile.clone()),
                ("NUM_JOBS".to_owned(), "1".to_owned()),
                ("RUSTC".to_owned(), spec.rustc_path.display().to_string()),
                (
                    "RUSTDOC".to_owned(),
                    spec.rustdoc_path.display().to_string(),
                ),
                (
                    "CARGO_ENCODED_RUSTFLAGS".to_owned(),
                    spec.encoded_rustflags.clone(),
                ),
                (
                    "CARGO".to_owned(),
                    std::env::current_exe()
                        .unwrap_or_default()
                        .display()
                        .to_string(),
                ),
                ("CARGO_PKG_NAME".to_owned(), spec.pkg_name.clone()),
                ("CARGO_PKG_VERSION".to_owned(), spec.pkg_version.clone()),
                (
                    "CARGO_MANIFEST_DIR".to_owned(),
                    format!("{EXEC_ROOT_VAR}/in"),
                ),
                (
                    "CARGO_MANIFEST_PATH".to_owned(),
                    format!("{EXEC_ROOT_VAR}/in/Cargo.toml"),
                ),
            ]);
            if let Some(links) = &spec.links {
                env.insert("CARGO_MANIFEST_LINKS".to_owned(), links.clone());
            }
            for (name, value) in &spec.cfgs {
                env.insert(
                    format!("CARGO_CFG_{}", name.to_ascii_uppercase()),
                    value.clone(),
                );
            }
            // DEP_<LINKS>_<KEY>: metadata exported by direct dependencies'
            // build scripts (Cargo namespaces metadata by the `links`
            // value of the exporting package).
            for (links, bs_id) in &spec.dep_links {
                let Some(stdout) = completed.stdout(bs_id) else {
                    continue;
                };
                let bytes = cas.read_blob(stdout)?;
                let directives = parse_directives(&String::from_utf8_lossy(&bytes));
                let prefix = format!("DEP_{}_", links.to_ascii_uppercase().replace('-', "_"));
                for (key, value) in &directives.metadata {
                    env.insert(
                        format!("{prefix}{}", key.to_ascii_uppercase().replace('-', "_")),
                        value.clone(),
                    );
                }
            }
            for feature in &spec.features {
                let key = format!(
                    "CARGO_FEATURE_{}",
                    feature.to_ascii_uppercase().replace('-', "_")
                );
                env.insert(key, "1".to_owned());
            }
            if let Ok(version) = semver::Version::parse(&spec.pkg_version) {
                env.insert(
                    "CARGO_PKG_VERSION_MAJOR".to_owned(),
                    version.major.to_string(),
                );
                env.insert(
                    "CARGO_PKG_VERSION_MINOR".to_owned(),
                    version.minor.to_string(),
                );
                env.insert(
                    "CARGO_PKG_VERSION_PATCH".to_owned(),
                    version.patch.to_string(),
                );
                env.insert("CARGO_PKG_VERSION_PRE".to_owned(), version.pre.to_string());
            }
        }
        CtxKind::TestRun(spec) => {
            for (name, action, _) in &spec.bin_env {
                let tree = completed
                    .output_tree(action)
                    .ok_or_else(|| PlanError::MissingDependency(action.clone()))?;
                let mount = RelativePath::new(&format!("bin-exe/{name}")).map_err(|err| {
                    PlanError::Message(format!("invalid binary name {name}: {err}"))
                })?;
                mounts.push((mount, tree));
            }
            if spec.doc {
                // Doc test: rustdoc compiles the crate from its root and
                // runs the doctests; dependencies are linked like a
                // compile unit.
                args.push("--test".to_owned());
                args.push(spec.crate_root.to_string_lossy().into_owned());
                args.push("--edition".to_owned());
                args.push(spec.edition.to_rustc().to_owned());
                for (index, dep) in spec.deps.iter().enumerate() {
                    match dep {
                        DepSpec::Rust {
                            extern_name,
                            action,
                            file,
                        } => {
                            let tree = completed
                                .output_tree(action)
                                .ok_or_else(|| PlanError::MissingDependency(action.clone()))?;
                            let mount = format!("deps/direct/{index}");
                            mounts.push((
                                RelativePath::new(&mount)
                                    .map_err(|error| PlanError::Message(error.to_string()))?,
                                tree,
                            ));
                            args.push("-L".to_owned());
                            args.push(format!("dependency={EXEC_ROOT_VAR}/in/{mount}"));
                            args.push("--extern".to_owned());
                            args.push(format!("{extern_name}={EXEC_ROOT_VAR}/in/{mount}/{file}"));
                        }
                        DepSpec::Native => {}
                    }
                }
                args.extend(spec.args.iter().cloned());
            } else {
                // The test binary runs with the package source tree as its
                // working directory (relative test data paths behave like
                // Cargo). The binary itself is resolved from the compile
                // action's output tree at execution time.
                args.extend(spec.args.iter().cloned());
                if !spec.harness {
                    // Custom harness: the binary is a plain program; it
                    // gets the source tree as working directory.
                    let _ = &spec.binary;
                }
            }
        }
        CtxKind::Compile(spec) => {
            let mut directives = Directives::default();
            if let Some(bs_id) = &spec.build_script {
                let out = completed
                    .output_tree(bs_id)
                    .ok_or_else(|| PlanError::MissingDependency(bs_id.clone()))?;
                mounts.push((RelativePath::new("build_out").unwrap(), out));
                if let Some(stdout) = completed.stdout(bs_id) {
                    let bytes = cas.read_blob(stdout)?;
                    directives = parse_directives(&String::from_utf8_lossy(&bytes));
                    if let Some(err) = directives.errors.first() {
                        return Err(PlanError::Message(format!(
                            "build script for {} failed: {err}",
                            spec.crate_name
                        )));
                    }
                    for warning in &directives.warnings {
                        eprintln!("warning: {}: {warning}", spec.crate_name);
                    }
                }
                env.insert(
                    "OUT_DIR".to_owned(),
                    format!("{EXEC_ROOT_VAR}/in/build_out"),
                );
            }
            env.insert(
                "CARGO_MANIFEST_DIR".to_owned(),
                format!("{EXEC_ROOT_VAR}/in"),
            );
            env.insert(
                "CARGO_MANIFEST_PATH".to_owned(),
                format!("{EXEC_ROOT_VAR}/in/Cargo.toml"),
            );
            for (name, action, output) in &spec.bin_env {
                let tree = completed
                    .output_tree(action)
                    .ok_or_else(|| PlanError::MissingDependency(action.clone()))?;
                let mount = RelativePath::new(&format!("bin-exe/{name}")).map_err(|err| {
                    PlanError::Message(format!("invalid binary name {name}: {err}"))
                })?;
                mounts.push((mount, tree));
                env.insert(
                    format!("CARGO_BIN_EXE_{name}"),
                    format!("bin-exe/{name}/{output}"),
                );
            }
            for (index, dep) in spec.deps.iter().enumerate() {
                match dep {
                    DepSpec::Rust {
                        extern_name,
                        action,
                        file,
                    } => {
                        let tree = completed
                            .output_tree(action)
                            .ok_or_else(|| PlanError::MissingDependency(action.clone()))?;
                        let mount = format!("deps/direct/{index}");
                        mounts.push((
                            RelativePath::new(&mount)
                                .map_err(|error| PlanError::Message(error.to_string()))?,
                            tree,
                        ));
                        args.push("-L".to_owned());
                        args.push(format!("dependency={EXEC_ROOT_VAR}/in/{mount}"));
                        args.push("--extern".to_owned());
                        args.push(format!("{extern_name}={EXEC_ROOT_VAR}/in/{mount}/{file}"));
                    }
                    DepSpec::Native => {}
                }
            }
            // Transitive crate outputs: rustc resolves them through
            // `-L dependency=...`; direct mounts above are a subset.
            let direct: BTreeSet<ActionId> = spec
                .deps
                .iter()
                .filter_map(|dep| match dep {
                    DepSpec::Rust { action, .. } => Some(action.clone()),
                    DepSpec::Native => None,
                })
                .collect();
            for (index, dep) in spec.transitive_deps.iter().enumerate() {
                if let DepSpec::Rust { action, .. } = dep {
                    if direct.contains(action) {
                        continue;
                    }
                    let tree = completed
                        .output_tree(action)
                        .ok_or_else(|| PlanError::MissingDependency(action.clone()))?;
                    let mount = format!("deps/transitive/{index}");
                    mounts.push((
                        RelativePath::new(&mount)
                            .map_err(|error| PlanError::Message(error.to_string()))?,
                        tree,
                    ));
                    args.push("-L".to_owned());
                    args.push(format!("dependency={EXEC_ROOT_VAR}/in/{mount}"));
                }
            }
            for (name, tree, link) in &ctx.cc {
                let mount = RelativePath::new(&format!("cc/{name}"))
                    .map_err(|err| PlanError::Message(format!("invalid cc mount {name}: {err}")))?;
                mounts.push((mount, *tree));
                args.push("-L".to_owned());
                args.push(format!("native={EXEC_ROOT_VAR}/in/cc/{name}"));
                args.push("-l".to_owned());
                args.push(link.clone());
            }
            let _build_out = format!("{EXEC_ROOT_VAR}/in/build_out");
            // Link directives from transitive dependency build scripts
            // (the own script is merged separately below). Libraries,
            // searches, and raw flags are deduplicated so a library linked
            // by several dependencies is passed once.
            let mut seen = SeenDirectives::default();
            for (index, source) in spec.directive_sources.iter().enumerate() {
                if spec.build_script.as_ref() == Some(source) {
                    continue;
                }
                let Some(stdout) = completed.stdout(source) else {
                    continue;
                };
                let bytes = cas.read_blob(stdout)?;
                let dep_directives = parse_directives(&String::from_utf8_lossy(&bytes));
                let mount = format!("build_out_deps/{index}");
                if let Some(tree) = completed.output_tree(source) {
                    mounts.push((
                        RelativePath::new(&mount)
                            .map_err(|error| PlanError::Message(error.to_string()))?,
                        tree,
                    ));
                }
                let output_mount = format!("{EXEC_ROOT_VAR}/in/{mount}");
                apply_directives(
                    &mut args,
                    &mut env,
                    &dep_directives,
                    &spec.crate_type,
                    &mut seen,
                    Some(&output_mount),
                    false,
                );
            }
            apply_directives(
                &mut args,
                &mut env,
                &directives,
                &spec.crate_type,
                &mut seen,
                Some(&format!("{EXEC_ROOT_VAR}/in/build_out")),
                true,
            );
            for cfg in &directives.cfgs {
                args.push("--cfg".to_owned());
                args.push(cfg.clone());
            }
            let mut extra_metadata = String::new();
            for value in &seen.extra_metadata {
                extra_metadata.push_str(value);
            }

            args.push("--crate-name".to_owned());
            args.push(spec.crate_name.clone());
            if spec.crate_type == "test" {
                // Test harness compile (`[[test]]` / lib unit tests).
                args.push("--test".to_owned());
            } else {
                args.push("--crate-type".to_owned());
                args.push(spec.crate_type.clone());
            }
            if spec.crate_type == "proc-macro" || (spec.is_proc_macro && spec.crate_type == "test")
            {
                // rustc only exposes the proc_macro crate to explicitly
                // requested externs (also needed by unit tests of
                // proc-macro crates).
                args.push("--extern".to_owned());
                args.push("proc_macro".to_owned());
            }
            args.push("--edition".to_owned());
            args.push(spec.edition.to_rustc().to_owned());
            args.push("-C".to_owned());
            args.push(format!("metadata={}{extra_metadata}", spec.meta));
            args.extend(ctx.profile_flags.iter().cloned());
            args.push("-o".to_owned());
            args.push(format!("{EXEC_ROOT_VAR}/out/{}", spec.output));
            // Dep-info emission with an explicit output path: combining
            // emit types would make rustc adapt (rename) the `-o` target.
            // The `.d` file lands in the output tree and lets later builds
            // narrow their inputs to the files rustc actually read.
            args.push("--emit".to_owned());
            args.push(if spec.check && !spec.is_proc_macro {
                // Proc macros cannot be checked: cargo builds them fully
                // even under `cargo check` (consumers link the dylib).
                "metadata".to_owned()
            } else {
                "link".to_owned()
            });
            args.push("--emit".to_owned());
            args.push(format!("dep-info={EXEC_ROOT_VAR}/out/{}.d", spec.output));
            if !spec.host_unit
                && let Some(triple) = &ctx.target_triple
            {
                args.push("--target".to_owned());
                args.push(triple.clone());
            }
            args.extend(spec.extra_flags.iter().cloned());
            args.extend(spec.feature_cfgs.iter().cloned());
            args.push(spec.crate_root.to_string_lossy().into_owned());
            // Compile-time Cargo environment (crates read these with
            // `env!` / `option_env!`).
            env.insert("CARGO_CRATE_NAME".to_owned(), spec.crate_name.clone());
            if spec.crate_type == "bin" {
                env.insert("CARGO_BIN_NAME".to_owned(), spec.output.clone());
            }
            if spec.primary {
                env.insert("CARGO_PRIMARY_PACKAGE".to_owned(), "1".to_owned());
            }
        }
    }

    let input_root = cas.assemble(&mounts)?;
    let declared_outputs =
        match &ctx.kind {
            CtxKind::Compile(spec) => vec![OutputPath::new(&spec.output).map_err(|err| {
                PlanError::Message(format!("invalid output {}: {err}", spec.output))
            })?],
            CtxKind::BuildScriptRun(_) | CtxKind::TestRun(_) => Vec::new(),
        };
    // Test runs are never cached unless the native manifest opted in via
    // `cache-test-result = true`. Run actions under `[policy] network =
    // "allow"` may reach the network and are therefore uncacheable.
    let cache_policy = match &ctx.kind {
        CtxKind::TestRun(spec) if spec.cache_test_result => CachePolicy::Enabled,
        CtxKind::TestRun(_) => CachePolicy::NoCache,
        _ if ctx.network_allow => CachePolicy::NoCache,
        _ => CachePolicy::Enabled,
    };

    Ok(ActionSpec {
        schema_version: ACTION_SCHEMA_VERSION,
        logical_id: ctx.logical_id.clone(),
        mnemonic: ctx.mnemonic.clone(),
        executable: ctx.executable(completed)?,
        arguments: args.into_iter().map(Argument).collect(),
        environment_bundle: ctx.bundle,
        environment: env,
        input_root,
        declared_outputs,
        working_directory: RelativePath::new(".").unwrap(),
        execution_platform: host_platform(),
        target_platform: None,
        timeout: None,
        network_policy: if ctx.network_allow {
            NetworkPolicy::Allow
        } else {
            NetworkPolicy::Deny
        },
        cache_policy,
        resource_requirements: ResourceRequirements::default(),
        properties: ctx.properties.clone(),
    })
}

/// Finds a `.d` dep-info blob inside a captured output tree.
fn find_dep_blob(tree: &Tree, cas: &Cas) -> Option<Vec<u8>> {
    for (name, entry) in tree.entries() {
        match entry {
            TreeEntry::File { digest, .. } => {
                if name.ends_with(".d") && name.len() > 2 {
                    return cas.read_blob(*digest).ok();
                }
            }
            TreeEntry::Directory(digest) => {
                if let Some(sub) = cas.get_tree(*digest).ok().flatten()
                    && let Some(found) = find_dep_blob(&sub, cas)
                {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

/// Parses rustc's makefile-format dep-info: `target: dep1 dep2 \\` with
/// escaped spaces and line continuations.
fn parse_dep_info(text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\\').trim();
        let line = line.split_once(':').map(|(_, rest)| rest).unwrap_or(line);
        let mut current = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(&next) = chars.peek() {
                    current.push(next);
                    chars.next();
                }
            } else if c.is_whitespace() {
                if !current.is_empty() {
                    out.push(PathBuf::from(std::mem::take(&mut current)));
                }
            } else {
                current.push(c);
            }
        }
        if !current.is_empty() {
            out.push(PathBuf::from(std::mem::take(&mut current)));
        }
    }
    out
}

/// Dedup state for directive application across transitive scripts.
#[derive(Default)]
struct SeenDirectives {
    libs: BTreeSet<(Option<String>, String)>,
    searches: BTreeSet<String>,
    flags: BTreeSet<String>,
    extra_metadata: Vec<String>,
}

/// Applies a build script's directives to a compile action's args and env.
/// Target-kind-specific link args apply per `crate_type`; the own script's
/// env pairs win over transitive ones (applied later).
fn apply_directives(
    args: &mut Vec<String>,
    env: &mut BTreeMap<String, String>,
    directives: &Directives,
    crate_type: &str,
    seen: &mut SeenDirectives,
    output_mount: Option<&str>,
    local: bool,
) {
    for (kind, lib) in &directives.link_libs {
        if seen.libs.insert((kind.clone(), lib.clone())) {
            args.push("-l".to_owned());
            match kind {
                Some(kind) => args.push(format!("{kind}={lib}")),
                None => args.push(lib.clone()),
            }
        }
    }
    for search in &directives.link_search {
        if seen.searches.insert(search.clone()) {
            args.push("-L".to_owned());
            args.push(link_search_arg(search, output_mount));
        }
    }
    for flag in &directives.raw_flags {
        if seen.flags.insert(flag.clone()) {
            args.push(flag.clone());
        }
    }
    // Link args only apply to targets that actually link (rlibs are
    // archives; passing linker flags into the rlib compile is an error).
    let links = matches!(crate_type, "bin" | "test" | "cdylib" | "staticlib");
    if links {
        for arg in &directives.link_args {
            args.push("-C".to_owned());
            args.push(format!("link-arg={arg}"));
        }
    }
    match crate_type {
        "bin" => {
            for arg in &directives.link_arg_bins {
                args.push("-C".to_owned());
                args.push(format!("link-arg={arg}"));
            }
        }
        "test" => {
            for arg in &directives.link_arg_tests {
                args.push("-C".to_owned());
                args.push(format!("link-arg={arg}"));
            }
        }
        "cdylib" => {
            for arg in &directives.cdylib_link_args {
                args.push("-C".to_owned());
                args.push(format!("link-arg={arg}"));
            }
        }
        _ => {}
    }
    if local {
        for cfg in &directives.check_cfgs {
            args.push("--check-cfg".to_owned());
            args.push(cfg.clone());
        }
    }
    for value in &directives.extra_metadata {
        if !seen.extra_metadata.contains(value) {
            seen.extra_metadata.push(value.clone());
        }
    }
    if local {
        for (key, value) in &directives.env {
            let value = if let Some(output_mount) = output_mount {
                remap_build_output_path(value, output_mount)
            } else {
                value.clone()
            };
            env.insert(key.clone(), value);
        }
    }
}

/// Build scripts commonly export an absolute file beneath `OUT_DIR` via
/// `cargo:rustc-env`. The producing exec root is ephemeral; consumers see
/// the captured output tree mounted at `in/build_out`, so rewrite that
/// prefix into the consuming action's exec root.
fn remap_build_output_path(value: &str, output_mount: &str) -> String {
    if !Path::new(value).is_absolute() {
        return value.to_owned();
    }
    if let Some(index) = value.rfind("/out/") {
        return format!("{output_mount}/{}", &value[index + 5..]);
    }
    if value.ends_with("/out") {
        return output_mount.to_owned();
    }
    value.to_owned()
}

fn link_search_arg(value: &str, output_mount: Option<&str>) -> String {
    // Relative search paths resolve against the package root (the input
    // root, where the source tree is mounted at "."); absolute paths pass
    // verbatim (Cargo semantics).
    if let Some((kind, path)) = value.split_once('=') {
        let path = if let Some(output_mount) = output_mount {
            remap_build_output_path(path, output_mount)
        } else {
            path.to_owned()
        };
        if path.starts_with('/') || path.starts_with(EXEC_ROOT_VAR) {
            return format!("{kind}={path}");
        }
        return format!("{kind}={EXEC_ROOT_VAR}/in/{path}");
    }
    if let Some(output_mount) = output_mount {
        let path = remap_build_output_path(value, output_mount);
        if path != value {
            return path;
        }
    }
    if value.starts_with('/') {
        return value.to_owned();
    }
    format!("{EXEC_ROOT_VAR}/in/{value}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaps_own_build_output_directives() {
        let old = "/workspace/.tong/exec/abc/out/generated.rs";
        let mount = format!("{EXEC_ROOT_VAR}/in/build_out");
        assert_eq!(
            remap_build_output_path(old, &mount),
            format!("{EXEC_ROOT_VAR}/in/build_out/generated.rs")
        );
        assert_eq!(
            link_search_arg("native=/workspace/.tong/exec/abc/out", Some(&mount)),
            format!("native={EXEC_ROOT_VAR}/in/build_out")
        );
    }

    #[test]
    fn transitive_build_directives_do_not_leak_local_cfg_or_env() {
        let directives = Directives {
            link_libs: vec![(None, "native".to_owned())],
            check_cfgs: vec!["cfg(feature,values(\"private\"))".to_owned()],
            env: vec![("PRIVATE".to_owned(), "value".to_owned())],
            ..Directives::default()
        };
        let mut args = Vec::new();
        let mut env = BTreeMap::new();
        apply_directives(
            &mut args,
            &mut env,
            &directives,
            "rlib",
            &mut SeenDirectives::default(),
            Some("{exec_root}/in/build_out_deps/0"),
            false,
        );

        assert_eq!(args, ["-l", "native"]);
        assert!(!env.contains_key("PRIVATE"));
    }
}
