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
use tong_core::canonical::Encoder;
use tong_core::paths::{OutputPath, RelativePath};
use tong_core::tree::{Tree, TreeEntry};
use tong_exec::EXEC_ROOT_VAR;
use tong_graph::{Completed, PlanError, PlannedAction};
use tong_store::{CAPTURE_EXCLUDES, Cas};

use crate::build_directives::{Directives, parse_directives};
use crate::model::{CrateType, Dep, Edition, Package, ProfileSpec, RustModel};
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
    kind: CtxKind,
    source_tree: TreeDigest,
    rustc: BlobDigest,
    bundle: Option<BundleRef>,
    properties: BTreeMap<String, CanonicalValue>,
    global_env: BTreeMap<String, String>,
    pkg_env: BTreeMap<String, String>,
    cc: Vec<(String, TreeDigest, String)>,
    profile_flags: Vec<String>,
}

enum CtxKind {
    Compile(CompileSpec),
    BuildScriptRun(BuildScriptRunSpec),
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
}

struct BuildScriptRunSpec {
    compile: ActionId,
    binary: String,
    pkg_name: String,
    pkg_version: String,
    host_triple: String,
    opt_level: String,
    debug: bool,
}

/// The Rust backend: plans actions from a [`RustModel`].
pub struct RustBackend<'a> {
    cas: Cas,
    model: &'a RustModel,
    toolchain: SystemRust,
    profile_name: String,
    profile: ProfileSpec,
    source_trees: BTreeMap<String, TreeDigest>,
    /// Original crate path (relative to the package dir) → rewritten path
    /// inside the source tree, for crate roots mounted outside the package
    /// dir.
    crate_roots: BTreeMap<(String, PathBuf), PathBuf>,
    cc: BTreeMap<String, CcInfo>,
    cc_closure: BTreeMap<String, Vec<String>>,
    planned_ids: BTreeMap<String, ActionId>,
}

impl<'a> RustBackend<'a> {
    /// Creates a backend using `profile_name` from the model's profile table.
    pub fn new(
        cas: Cas,
        model: &'a RustModel,
        toolchain: SystemRust,
        profile_name: &str,
    ) -> Result<Self, PlanError> {
        let profile = model
            .profiles
            .get(profile_name)
            .cloned()
            .ok_or_else(|| PlanError::Message(format!("unknown profile {profile_name:?}")))?;
        Ok(Self {
            cas,
            model,
            toolchain,
            profile_name: profile_name.to_owned(),
            profile,
            source_trees: BTreeMap::new(),
            crate_roots: BTreeMap::new(),
            cc: BTreeMap::new(),
            cc_closure: BTreeMap::new(),
            planned_ids: BTreeMap::new(),
        })
    }

    /// Captures inputs and produces the planned action graph.
    pub fn plan(&mut self) -> Result<Vec<PlannedAction>, PlanError> {
        // 1. Capture package source trees once (PLAN.md section 8.3: whole
        //    package tree, excluding known output directories).
        for pkg in &self.model.packages {
            let excludes = CAPTURE_EXCLUDES.iter().copied().collect();
            let mut tree = self.cas.capture_dir_filtered(&pkg.dir, &excludes)?;

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
                    .assemble(&[(RelativePath::new(".").unwrap(), tree), (mount, ext_tree)])?;
                let relative = canonical.strip_prefix(parent).map_err(|_| {
                    PlanError::Message(format!(
                        "cannot relativize external crate root {}",
                        canonical.display()
                    ))
                })?;
                let rewritten = PathBuf::from(format!("ext/{index}")).join(relative);
                self.crate_roots.insert((pkg.name.clone(), path), rewritten);
                index += 1;
            }

            self.source_trees.insert(pkg.name.clone(), tree);
        }

        // 2. Import prebuilt native libraries.
        for import in &self.model.cc_imports {
            self.import_cc(import)?;
        }

        // 3. Compute transitive native closures per package.
        let pkg_map: BTreeMap<&str, &Package> = self
            .model
            .packages
            .iter()
            .map(|pkg| (pkg.name.as_str(), pkg))
            .collect();
        for pkg in &self.model.packages {
            let closure = self.collect_cc(&pkg.name, &pkg_map);
            self.cc_closure.insert(pkg.name.clone(), closure);
        }

        // 4. Build-script action ids are deterministic
        //    (`rust:bs-run:<pkg>`); pre-register them (for packages that
        //    have a build script) so any package can reference its
        //    dependencies' build-script directives while planning,
        //    regardless of package order.
        for pkg in &self.model.packages {
            if pkg.build_script.is_some() {
                self.planned_ids.insert(
                    format!("bs-run:{}", pkg.name),
                    ActionId(format!("rust:bs-run:{}", pkg.name)),
                );
            }
        }

        // 5. Plan actions. Libraries, proc macros, and build scripts first
        //    (their planned ids must exist before binaries resolve their
        //    dependency actions), then binaries.
        let mut actions = Vec::new();
        for pkg in &self.model.packages {
            self.plan_package_library(&mut actions, pkg)?;
        }
        for pkg in &self.model.packages {
            self.plan_package_bins(&mut actions, pkg)?;
        }

        Ok(actions)
    }

    /// Plans a package's build-script, library, and proc-macro actions.
    fn plan_package_library(
        &mut self,
        actions: &mut Vec<PlannedAction>,
        pkg: &Package,
    ) -> Result<Option<ActionId>, PlanError> {
        let source_tree = self.source_trees[&pkg.name];
        let cc = self.cc_for(&pkg.name);

        // Build script: compile, then run.
        let mut bs_run: Option<ActionId> = None;
        if let Some(script) = &pkg.build_script {
            let script = self.crate_root_for(&pkg.name, script);
            let binary = format!("{}_build_script", crate_name(&pkg.name));
            let compile_id = self.plan_compile(
                actions,
                &format!("bs-compile:{}", pkg.name),
                &format!("rust:bs-compile:{}", pkg.name),
                "RustBuildScriptCompile",
                pkg,
                source_tree,
                cc.clone(),
                binary.clone(),
                "bin",
                None,
                &pkg.build_deps,
                None,
                script.clone(),
            )?;

            let run_id = ActionId(format!("rust:bs-run:{}", pkg.name));
            let run_ctx = Ctx {
                logical_id: run_id.clone(),
                mnemonic: "RustBuildScriptRun".to_owned(),
                kind: CtxKind::BuildScriptRun(BuildScriptRunSpec {
                    compile: compile_id.clone(),
                    binary: binary.clone(),
                    pkg_name: pkg.name.clone(),
                    pkg_version: pkg.version.clone(),
                    host_triple: self.toolchain.host_triple.clone(),
                    opt_level: self.profile.opt_level.clone(),
                    debug: self.profile.debug,
                }),
                source_tree,
                rustc: self.toolchain.rustc_blob,
                bundle: Some(self.toolchain.bundle_ref()),
                properties: self.base_properties(),
                global_env: self.model.global_env.clone(),
                pkg_env: pkg.env.clone(),
                cc: Vec::new(),
                profile_flags: self.profile.rustc_flags(),
            };
            self.planned_ids
                .insert(format!("bs-run:{}", pkg.name), run_id.clone());
            actions.push(self.boxed(run_ctx));

            bs_run = Some(run_id);
        }

        // Library / proc-macro actions.
        if let Some(lib) = &pkg.lib {
            let lib_name = lib_crate_name(pkg);
            if lib.proc_macro {
                self.plan_compile(
                    actions,
                    &format!("lib:{}:proc-macro", pkg.name),
                    &format!("rust:proc-macro:{}", pkg.name),
                    "RustProcMacro",
                    pkg,
                    source_tree,
                    cc.clone(),
                    lib_name,
                    "proc-macro",
                    None,
                    &pkg.deps,
                    bs_run.clone(),
                    self.crate_root_for(&pkg.name, &lib.path),
                )?;
            } else {
                let types: Vec<CrateType> = if lib.crate_types.is_empty() {
                    vec![CrateType::Rlib]
                } else {
                    lib.crate_types.clone()
                };
                let lib_root = self.crate_root_for(&pkg.name, &lib.path);
                for crate_type in types {
                    self.plan_compile(
                        actions,
                        &format!("lib:{}:{}", pkg.name, crate_type.to_rustc()),
                        &format!("rust:lib:{}:{}", pkg.name, crate_type.to_rustc()),
                        "RustLibrary",
                        pkg,
                        source_tree,
                        cc.clone(),
                        lib_name.clone(),
                        crate_type.to_rustc(),
                        None,
                        &pkg.deps,
                        bs_run.clone(),
                        lib_root.clone(),
                    )?;
                }
            }
        }

        Ok(bs_run)
    }

    /// Plans a package's binary actions; each depends on the package's own
    /// library (when present) plus declared deps.
    fn plan_package_bins(
        &mut self,
        actions: &mut Vec<PlannedAction>,
        pkg: &Package,
    ) -> Result<(), PlanError> {
        let source_tree = self.source_trees[&pkg.name];
        let cc = self.cc_for(&pkg.name);
        let bs_run: Option<ActionId> = self
            .planned_ids
            .get(&format!("bs-run:{}", pkg.name))
            .cloned();

        for bin in &pkg.bins {
            let mut deps = pkg.deps.clone();
            if pkg.lib.is_some() {
                deps.insert(
                    0,
                    Dep {
                        extern_name: lib_crate_name(pkg),
                        package: pkg.name.clone(),
                        optional: false,
                        default_features: true,
                        features: Vec::new(),
                    },
                );
            }
            self.plan_compile(
                actions,
                &format!("bin:{}:{}", pkg.name, bin.name),
                &format!("rust:bin:{}:{}", pkg.name, bin.name),
                "RustBinary",
                pkg,
                source_tree,
                cc.clone(),
                crate_name(&bin.name),
                "bin",
                Some(bin.name.clone()),
                &deps,
                bs_run.clone(),
                self.crate_root_for(&pkg.name, &bin.path),
            )?;
        }
        Ok(())
    }

    /// Final runnable artifacts (binaries) with their runtime closures.
    pub fn final_artifacts(&self) -> Vec<FinalArtifact> {
        let mut out = Vec::new();
        for pkg in &self.model.packages {
            let runtime: Vec<(BlobDigest, String)> = self
                .cc_closure
                .get(&pkg.name)
                .unwrap()
                .iter()
                .map(|name| {
                    let info = &self.cc[name];
                    (info.blob, info.file_name.clone())
                })
                .collect();
            for bin in &pkg.bins {
                let Some(action) = self
                    .planned_ids
                    .get(&format!("bin:{}:{}", pkg.name, bin.name))
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
    ) -> Result<ActionId, PlanError> {
        let meta = self.metadata(&crate_name, crate_type);
        let output = if let Some(name) = output_name {
            name
        } else if crate_type == "bin" {
            crate_name.clone()
        } else {
            let ext = if crate_type == "proc-macro" {
                dll_extension()
            } else {
                match crate_type {
                    "rlib" => "rlib",
                    "staticlib" => "a",
                    _ => dll_extension(),
                }
            };
            format!("lib{crate_name}-{meta}.{ext}")
        };
        let dep_specs = self.resolve_deps(&pkg.name, deps)?;
        // Per-crate feature cfgs: `--cfg feature="<name>"` for every
        // activated feature (sorted), mirroring Cargo.
        let feature_cfgs: Vec<String> = self
            .model
            .feature_map
            .packages
            .get(&pkg.name)
            .map(|features| {
                features
                    .iter()
                    .flat_map(|feature| ["--cfg".to_owned(), format!("feature=\"{feature}\"")])
                    .collect()
            })
            .unwrap_or_default();
        let extra_flags: Vec<String> = self
            .model
            .global_rustflags
            .iter()
            .chain(pkg.rustflags.iter())
            .cloned()
            .collect();
        // LTO is not supported for proc-macro crate types; Cargo disables it
        // automatically.
        let mut profile_flags = self.profile.rustc_flags();
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
        let ctx = Ctx {
            logical_id: ActionId(logical_id.to_owned()),
            mnemonic: mnemonic.to_owned(),
            kind: CtxKind::Compile(CompileSpec {
                crate_name,
                edition: pkg.edition,
                crate_type: crate_type.to_owned(),
                meta,
                output,
                deps: dep_specs,
                build_script,
                directive_sources: self.link_directive_sources(pkg),
                crate_root,
                extra_flags,
                feature_cfgs,
            }),
            source_tree,
            rustc: self.toolchain.rustc_blob,
            bundle: Some(self.toolchain.bundle_ref()),
            properties: self.base_properties(),
            global_env: self.model.global_env.clone(),
            pkg_env: pkg.env.clone(),
            cc,
            profile_flags: profile_flags.clone(),
        };
        let id = ctx.logical_id.clone();
        self.planned_ids.insert(key.to_owned(), id.clone());
        actions.push(self.boxed(ctx));
        Ok(id)
    }

    fn boxed(&self, ctx: Ctx) -> PlannedAction {
        let logical_id = ctx.logical_id.clone();
        let mnemonic = ctx.mnemonic.clone();
        PlannedAction {
            logical_id,
            mnemonic,
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

    /// Deterministic per-crate metadata id; producers and consumers of an
    /// artifact derive the same value.
    fn metadata(&self, crate_name: &str, kind: &str) -> String {
        let mut enc = Encoder::new();
        enc.write_str(crate_name);
        enc.write_str(&self.profile_name);
        enc.write_str(kind);
        enc.write_str(&self.toolchain.host_triple);
        enc.digest().to_hex()[..16].to_owned()
    }

    /// The compile-time crate root for a package-relative path, rewritten
    /// when the file lives outside the package dir (mounted at `ext/<n>`).
    fn crate_root_for(&self, pkg: &str, original: &Path) -> PathBuf {
        self.crate_roots
            .get(&(pkg.to_owned(), original.to_path_buf()))
            .cloned()
            .unwrap_or_else(|| original.to_path_buf())
    }

    /// Build-script run ids whose link directives apply to `pkg`'s crates:
    /// the package's own script plus the scripts of every transitive
    /// dependency (Cargo propagates link directives to all dependents).
    /// Inactive optional dep edges are skipped.
    fn link_directive_sources(&self, pkg: &Package) -> Vec<ActionId> {
        fn active_deps<'b>(model: &RustModel, pkg: &'b Package) -> Vec<&'b Dep> {
            pkg.deps
                .iter()
                .chain(pkg.build_deps.iter())
                .filter(|dep| {
                    if !dep.optional {
                        return true;
                    }
                    model
                        .feature_map
                        .active_optional_deps
                        .get(&pkg.name)
                        .is_some_and(|active| active.contains(&dep.extern_name))
                })
                .collect()
        }

        let mut out = Vec::new();
        if let Some(id) = self.planned_ids.get(&format!("bs-run:{}", pkg.name)) {
            out.push(id.clone());
        }
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut stack: Vec<String> = active_deps(self.model, pkg)
            .iter()
            .map(|dep| dep.package.clone())
            .collect();
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(id) = self.planned_ids.get(&format!("bs-run:{name}")) {
                out.push(id.clone());
            }
            if let Some(dep) = self.model.packages.iter().find(|p| p.name == name) {
                stack.extend(
                    active_deps(self.model, dep)
                        .iter()
                        .map(|d| d.package.clone()),
                );
            }
        }
        out
    }

    fn resolve_deps(&self, pkg_name: &str, deps: &[Dep]) -> Result<Vec<DepSpec>, PlanError> {
        deps.iter()
            .filter(|dep| {
                if !dep.optional {
                    return true;
                }
                // Optional edges are linked only when activated by the
                // feature resolution (Cargo semantics).
                self.model
                    .feature_map
                    .active_optional_deps
                    .get(pkg_name)
                    .is_some_and(|active| active.contains(&dep.extern_name))
            })
            .map(|dep| self.dep_spec(dep))
            .collect()
    }

    /// Resolves a dependency to its producer action or native import.
    /// An unresolvable dependency is a plan error — silently dropping it
    /// would link against a missing crate and fail deep inside rustc.
    fn dep_spec(&self, dep: &Dep) -> Result<DepSpec, PlanError> {
        if self.cc.contains_key(&dep.package) {
            return Ok(DepSpec::Native);
        }
        let pkg = self
            .model
            .packages
            .iter()
            .find(|p| p.name == dep.package)
            .ok_or_else(|| {
                PlanError::Message(format!(
                    "dependency {:?} of {:?} names no imported package or cc_import \
                     (registry dependencies are unsupported until Phase 3 locking)",
                    dep.extern_name, dep.package
                ))
            })?;
        let (kind, ext, key) = if let Some(lib) = &pkg.lib {
            if lib.proc_macro {
                (
                    "proc-macro",
                    dll_extension(),
                    format!("lib:{}:proc-macro", pkg.name),
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
                    format!("lib:{}:{}", pkg.name, chosen.to_rustc()),
                )
            }
        } else {
            return Err(PlanError::Message(format!(
                "dependency {:?} names package {:?}, which has no library target",
                dep.extern_name, dep.package
            )));
        };
        let action = self
            .planned_ids
            .get(&key)
            .cloned()
            .ok_or_else(|| PlanError::Message(format!("no planned action for {key}")))?;
        let lib_name = lib_crate_name(pkg);
        let meta = self.metadata(&lib_name, kind);
        Ok(DepSpec::Rust {
            extern_name: dep.extern_name.clone(),
            action,
            file: format!("lib{lib_name}-{meta}.{ext}"),
        })
    }

    /// Transitive native closure of a package: every `cc_import` reachable
    /// through the dependency graph (inactive optional edges skipped).
    fn collect_cc(&self, pkg_name: &str, pkg_map: &BTreeMap<&str, &Package>) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut stack = vec![pkg_name.to_owned()];
        while let Some(name) = stack.pop() {
            let Some(current) = pkg_map.get(name.as_str()) else {
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
                        .active_optional_deps
                        .get(&current.name)
                        .is_some_and(|active| active.contains(&dep.extern_name))
                })
            {
                if self.cc.contains_key(&dep.package) {
                    if seen.insert(dep.package.clone()) {
                        out.push(dep.package.clone());
                    }
                } else if pkg_map.contains_key(dep.package.as_str()) {
                    stack.push(dep.package.clone());
                }
            }
        }
        out.sort();
        out
    }

    fn cc_for(&self, pkg_name: &str) -> Vec<(String, TreeDigest, String)> {
        self.cc_closure
            .get(pkg_name)
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
                .filter_map(|dep| match dep {
                    DepSpec::Rust { action, .. } => Some(action.clone()),
                    DepSpec::Native => None,
                })
                .chain(spec.build_script.clone())
                .collect(),
            CtxKind::BuildScriptRun(spec) => vec![spec.compile.clone()],
        };
        deps.sort();
        deps.dedup();
        deps
    }

    /// The executable artifact, resolved once dependencies complete (a
    /// build-script binary lives inside its compile action's output tree).
    fn executable(&self, completed: &dyn Completed) -> Result<ArtifactRef, PlanError> {
        match &self.kind {
            CtxKind::Compile(_) => Ok(ArtifactRef::Blob(self.rustc)),
            CtxKind::BuildScriptRun(spec) => {
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

/// Concretizes a planned action into a full `ActionSpec`.
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
                ("TARGET".to_owned(), spec.host_triple.clone()),
                ("HOST".to_owned(), spec.host_triple.clone()),
                ("OPT_LEVEL".to_owned(), spec.opt_level.clone()),
                ("DEBUG".to_owned(), spec.debug.to_string()),
                ("NUM_JOBS".to_owned(), "1".to_owned()),
                ("CARGO_PKG_NAME".to_owned(), spec.pkg_name.clone()),
                ("CARGO_PKG_VERSION".to_owned(), spec.pkg_version.clone()),
                (
                    "CARGO_MANIFEST_DIR".to_owned(),
                    format!("{EXEC_ROOT_VAR}/in"),
                ),
            ]);
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
            for dep in &spec.deps {
                match dep {
                    DepSpec::Rust {
                        extern_name,
                        action,
                        file,
                    } => {
                        let tree = completed
                            .output_tree(action)
                            .ok_or_else(|| PlanError::MissingDependency(action.clone()))?;
                        mounts.push((RelativePath::new("deps").unwrap(), tree));
                        args.push("--extern".to_owned());
                        args.push(format!("{extern_name}={EXEC_ROOT_VAR}/in/deps/{file}"));
                    }
                    DepSpec::Native => {}
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
            let build_out = format!("{EXEC_ROOT_VAR}/in/build_out");
            // Link directives from transitive dependency build scripts
            // (the own script is already merged above). Deduplicated so a
            // library linked by several dependencies is passed once.
            let mut seen_libs: BTreeSet<String> = BTreeSet::new();
            let mut seen_searches: BTreeSet<String> = BTreeSet::new();
            let mut seen_flags: BTreeSet<String> = BTreeSet::new();
            for source in &spec.directive_sources {
                if spec.build_script.as_ref() == Some(source) {
                    continue;
                }
                let Some(stdout) = completed.stdout(source) else {
                    continue;
                };
                let bytes = cas.read_blob(stdout)?;
                let dep_directives = parse_directives(&String::from_utf8_lossy(&bytes));
                for lib in dep_directives.link_libs {
                    if seen_libs.insert(lib.clone()) {
                        args.push("-l".to_owned());
                        args.push(lib);
                    }
                }
                for search in dep_directives.link_search {
                    if seen_searches.insert(search.clone()) {
                        args.push("-L".to_owned());
                        args.push(link_search_arg(&search, &build_out));
                    }
                }
                for flag in dep_directives.raw_flags {
                    if seen_flags.insert(flag.clone()) {
                        args.push(flag);
                    }
                }
                for (key, value) in dep_directives.env {
                    env.insert(key, value);
                }
            }
            for flag in &directives.raw_flags {
                args.push(flag.clone());
            }
            for cfg in &directives.cfgs {
                args.push("--cfg".to_owned());
                args.push(cfg.clone());
            }
            for lib in &directives.link_libs {
                args.push("-l".to_owned());
                args.push(lib.clone());
            }
            for search in &directives.link_search {
                args.push("-L".to_owned());
                args.push(link_search_arg(search, &build_out));
            }
            for (key, value) in &directives.env {
                env.insert(key.clone(), value.clone());
            }

            args.push("--crate-name".to_owned());
            args.push(spec.crate_name.clone());
            args.push("--crate-type".to_owned());
            args.push(spec.crate_type.clone());
            if spec.crate_type == "proc-macro" {
                // rustc only exposes the proc_macro crate to explicitly
                // requested externs.
                args.push("--extern".to_owned());
                args.push("proc_macro".to_owned());
            }
            args.push("--edition".to_owned());
            args.push(spec.edition.to_rustc().to_owned());
            args.push("-C".to_owned());
            args.push(format!("metadata={}", spec.meta));
            args.extend(ctx.profile_flags.iter().cloned());
            args.push("-o".to_owned());
            args.push(format!("{EXEC_ROOT_VAR}/out/{}", spec.output));
            args.push("-L".to_owned());
            args.push(format!("dependency={EXEC_ROOT_VAR}/in/deps"));
            args.extend(spec.extra_flags.iter().cloned());
            args.extend(spec.feature_cfgs.iter().cloned());
            args.push(spec.crate_root.to_string_lossy().into_owned());
        }
    }

    let input_root = cas.assemble(&mounts)?;
    let declared_outputs =
        match &ctx.kind {
            CtxKind::Compile(spec) => vec![OutputPath::new(&spec.output).map_err(|err| {
                PlanError::Message(format!("invalid output {}: {err}", spec.output))
            })?],
            CtxKind::BuildScriptRun(_) => Vec::new(),
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
        network_policy: NetworkPolicy::Deny,
        cache_policy: CachePolicy::Enabled,
        resource_requirements: ResourceRequirements::default(),
        properties: ctx.properties.clone(),
    })
}

fn link_search_arg(value: &str, build_out: &str) -> String {
    if let Some((kind, path)) = value.split_once('=') {
        if path.starts_with('/') {
            return format!("{kind}={path}");
        }
        return format!("{kind}={build_out}/{path}");
    }
    if value.starts_with('/') {
        return value.to_owned();
    }
    format!("{build_out}/{value}")
}

/// The crate name of a package's library: the `[lib] name` override when
/// present, else the sanitized package name. Binaries and dependents must
/// agree on this for `--crate-name` and the rlib filename.
fn lib_crate_name(pkg: &Package) -> String {
    match &pkg.lib {
        Some(lib) => lib
            .name
            .as_deref()
            .map(crate_name)
            .unwrap_or_else(|| crate_name(&pkg.name)),
        None => crate_name(&pkg.name),
    }
}

fn crate_name(name: &str) -> String {
    name.replace('-', "_")
}
