use crate::args::{
    BuiltLib, add_dependency_args, add_feature_args, add_metadata_args, add_profile_args,
    opt_level, rust_lib_output_name,
};
use crate::build_script::{
    BuildScriptOutput, add_build_script_args, build_script_env, parse_build_script_stdout,
};
use crate::dep_info::parse_makefile_dep_info;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tong_core::action::Action;
use tong_core::cache::ActionCache;
use tong_core::env::EnvBundle;
use tong_core::error::{IoContext, Result, TongError};
use tong_core::exec::Executor;
use tong_core::hash;
use tong_core::language::{BuildOutput, BuildProfile, BuildRequest, LanguageBackend};
use tong_core::paths;
use tong_graph::{PackageKey, PackageNode, ProjectGraph};
use tong_manifest::{BinTarget, ExampleTarget, LibTarget, TestTarget};

#[derive(Debug)]
pub struct RustBackend {
    rustc: PathBuf,
    linker: Option<PathBuf>,
    rustc_version: String,
    built_libs: BTreeMap<PackageKey, BuiltLib>,
    build_scripts: BTreeMap<PackageKey, Option<BuildScriptOutput>>,
    building: BTreeSet<PackageKey>,
}

struct RustActionSpec<'a> {
    id: String,
    mnemonic: &'a str,
    args: Vec<String>,
    inputs: Vec<PathBuf>,
    outputs: Vec<PathBuf>,
    workdir: &'a Path,
    request: &'a BuildRequest,
    extra_env: &'a BTreeMap<String, String>,
}

impl RustBackend {
    pub fn new() -> Result<Self> {
        let rustc = resolve_rustc()?;
        let rustc_version = rustc_version(&rustc)?;
        let linker = default_linker(&rustc_version);
        Ok(Self {
            rustc,
            linker,
            rustc_version,
            built_libs: BTreeMap::new(),
            build_scripts: BTreeMap::new(),
            building: BTreeSet::new(),
        })
    }

    fn build_root(&mut self, graph: &ProjectGraph, request: &BuildRequest) -> Result<BuildOutput> {
        let executor = Executor {
            cache: ActionCache::new(request.out_dir.join("cache/actions")),
            workspace_root: request
                .manifest_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            build_state_root: Some(request.out_dir.clone()),
            verbose: request.verbose,
        };

        let mut artifacts = Vec::new();
        let root = graph.package(&graph.root)?;

        let root_lib = self.build_package_lib(graph, &graph.root, request, &executor)?;
        if let Some(lib) = &root_lib {
            artifacts.push(lib.path.clone());
        }

        for bin in &root.manifest.bins {
            if !target_enabled(&bin.required_features, &root.features) {
                continue;
            }
            let path = self.build_bin(graph, root, bin, root_lib.as_ref(), request, &executor)?;
            artifacts.push(path);
        }
        if request.build_examples {
            for example in &root.manifest.examples {
                if !target_enabled(&example.required_features, &root.features) {
                    continue;
                }
                let bin = example_to_bin(example);
                let path =
                    self.build_bin(graph, root, &bin, root_lib.as_ref(), request, &executor)?;
                artifacts.push(path);
            }
        }
        if request.build_tests {
            for test in &root.manifest.tests {
                if !target_enabled(&test.required_features, &root.features) {
                    continue;
                }
                let bin = test_to_bin(test);
                let path =
                    self.build_bin(graph, root, &bin, root_lib.as_ref(), request, &executor)?;
                artifacts.push(path);
            }
        }

        Ok(BuildOutput { artifacts })
    }

    fn build_package_lib(
        &mut self,
        graph: &ProjectGraph,
        key: &PackageKey,
        request: &BuildRequest,
        executor: &Executor,
    ) -> Result<Option<BuiltLib>> {
        if let Some(lib) = self.built_libs.get(key) {
            return Ok(Some(lib.clone()));
        }
        if !self.building.insert(key.clone()) {
            let package = graph.package(key)?.manifest.package.name.clone();
            return Err(TongError::Cycle { package });
        }

        let node = graph.package(key)?;
        let mut dep_libs = Vec::new();
        for dependency in &node.dependencies {
            let Some(lib) = self.build_package_lib(graph, &dependency.key, request, executor)?
            else {
                return Err(TongError::unsupported(format!(
                    "dependency `{}` of package `{}` does not define a library target",
                    dependency.alias, node.manifest.package.name
                )));
            };
            dep_libs.push((dependency.alias.clone(), lib));
        }
        let mut build_dep_libs = Vec::new();
        for dependency in &node.build_dependencies {
            let Some(lib) = self.build_package_lib(graph, &dependency.key, request, executor)?
            else {
                return Err(TongError::unsupported(format!(
                    "build-dependency `{}` of package `{}` does not define a library target",
                    dependency.alias, node.manifest.package.name
                )));
            };
            build_dep_libs.push((dependency.alias.clone(), lib));
        }

        let build_script = self.run_build_script(node, &build_dep_libs, request, executor)?;
        let output = match &node.manifest.lib {
            Some(lib) => {
                let built = self.compile_lib(
                    node,
                    lib,
                    &dep_libs,
                    build_script.as_ref(),
                    request,
                    executor,
                )?;
                self.built_libs.insert(key.clone(), built.clone());
                Some(built)
            }
            None => None,
        };

        self.building.remove(key);
        Ok(output)
    }

    fn compile_lib(
        &self,
        node: &PackageNode,
        lib: &LibTarget,
        dependencies: &[(String, BuiltLib)],
        build_script: Option<&BuildScriptOutput>,
        request: &BuildRequest,
        executor: &Executor,
    ) -> Result<BuiltLib> {
        let crate_name = paths::normalize_crate_name(&lib.name);
        let package_hash = hash::hash_bytes(node.key.0.to_string_lossy().as_bytes());
        let short_hash = &package_hash[..8];
        let crate_type = if lib.proc_macro { "proc-macro" } else { "lib" };
        let metadata_hash = metadata_hash(node, request.profile, &self.host_triple(), crate_type);
        let short_metadata = &metadata_hash[..8];
        let output = request
            .out_dir
            .join(request.profile.as_str())
            .join("deps")
            .join(rust_lib_output_name(
                &crate_name,
                short_hash,
                lib.proc_macro,
                short_metadata,
            ));
        let dep_info = dep_info_path(request, &format!("lib-{}", node.manifest.package.name));
        let mut args = vec![
            "--crate-name".to_owned(),
            crate_name.clone(),
            "--edition".to_owned(),
            node.manifest.package.edition.clone(),
            "--crate-type".to_owned(),
            crate_type.to_owned(),
            paths::display_path(&lib.path),
            "-o".to_owned(),
            paths::display_path(&output),
            "--emit".to_owned(),
            format!("link,dep-info={}", paths::display_path(&dep_info)),
        ];
        add_feature_args(&node.features, &mut args);
        add_profile_args(request.profile, &mut args);
        add_metadata_args(short_metadata, &mut args);
        add_dependency_args(dependencies, &mut args);
        add_build_script_args(build_script, &mut args);
        if lib.proc_macro {
            args.push("--extern".to_owned());
            args.push("proc_macro".to_owned());
            if let Some(linker) = &self.linker {
                args.push("-C".to_owned());
                args.push(format!("linker={}", paths::display_path(linker)));
            }
        }

        let mut inputs = package_inputs_with_dep_info(node, &dep_info)?;
        for (_, dependency) in dependencies {
            inputs.push(dependency.path.clone());
        }
        if let Some(build_script) = build_script {
            inputs.extend(build_script.generated_inputs.clone());
            inputs.push(build_script.stdout.clone());
        }

        let extra_env = build_script_env(build_script);

        let action = self.rust_action(RustActionSpec {
            id: format!("rust-lib:{}", node.manifest.package.name),
            mnemonic: "RustLib",
            args,
            inputs,
            outputs: vec![output.clone(), dep_info],
            workdir: &node.manifest.root,
            request,
            extra_env: &extra_env,
        })?;
        executor.run(&action)?;

        Ok(BuiltLib {
            extern_name: crate_name,
            path: output,
        })
    }

    fn build_bin(
        &mut self,
        graph: &ProjectGraph,
        node: &PackageNode,
        bin: &BinTarget,
        root_lib: Option<&BuiltLib>,
        request: &BuildRequest,
        executor: &Executor,
    ) -> Result<PathBuf> {
        let mut dependencies = Vec::new();
        for dependency in &node.dependencies {
            let lib = self.built_libs.get(&dependency.key).ok_or_else(|| {
                TongError::unsupported(format!(
                    "internal error: dependency `{}` was not built before binary",
                    dependency.alias
                ))
            })?;
            dependencies.push((dependency.alias.clone(), lib.clone()));
        }

        if let Some(root_lib) = root_lib {
            dependencies.push((root_lib.extern_name.clone(), root_lib.clone()));
        }

        let build_script = self.run_build_script(node, &[], request, executor)?;

        let crate_name = paths::normalize_crate_name(&bin.name);
        let output = request
            .out_dir
            .join(request.profile.as_str())
            .join("bin")
            .join(paths::executable_name(&bin.name));
        let dep_info = dep_info_path(request, &format!("bin-{}", bin.name));

        let mut args = vec![
            "--crate-name".to_owned(),
            crate_name,
            "--edition".to_owned(),
            node.manifest.package.edition.clone(),
            "--crate-type".to_owned(),
            "bin".to_owned(),
            paths::display_path(&bin.path),
            "-o".to_owned(),
            paths::display_path(&output),
            "--emit".to_owned(),
            format!("link,dep-info={}", paths::display_path(&dep_info)),
        ];
        add_feature_args(&node.features, &mut args);
        add_profile_args(request.profile, &mut args);
        add_metadata_args(
            &metadata_hash(node, request.profile, &self.host_triple(), "bin")[..8],
            &mut args,
        );
        add_dependency_args(&dependencies, &mut args);
        add_build_script_args(build_script.as_ref(), &mut args);

        if let Some(linker) = &self.linker {
            args.push("-C".to_owned());
            args.push(format!("linker={}", paths::display_path(linker)));
        }

        let mut inputs = package_inputs_with_dep_info(node, &dep_info)?;
        for (_, dependency) in dependencies {
            inputs.push(dependency.path);
        }
        if let Some(build_script) = &build_script {
            inputs.extend(build_script.generated_inputs.clone());
            inputs.push(build_script.stdout.clone());
        }

        let extra_env = build_script_env(build_script.as_ref());

        let action = self.rust_action(RustActionSpec {
            id: format!(
                "rust-bin:{}:{}",
                graph.package(&graph.root)?.manifest.package.name,
                bin.name
            ),
            mnemonic: "RustBin",
            args,
            inputs,
            outputs: vec![output.clone(), dep_info],
            workdir: &node.manifest.root,
            request,
            extra_env: &extra_env,
        })?;
        executor.run(&action)?;

        Ok(output)
    }

    fn run_build_script(
        &mut self,
        node: &PackageNode,
        build_dependencies: &[(String, BuiltLib)],
        request: &BuildRequest,
        executor: &Executor,
    ) -> Result<Option<BuildScriptOutput>> {
        if let Some(output) = self.build_scripts.get(&node.key) {
            return Ok(output.clone());
        }

        let Some(script_path) = &node.manifest.build_script else {
            self.build_scripts.insert(node.key.clone(), None);
            return Ok(None);
        };

        let script_bin =
            self.compile_build_script(node, script_path, build_dependencies, request, executor)?;
        let output = self.execute_build_script(node, &script_bin, request, executor)?;
        self.build_scripts
            .insert(node.key.clone(), Some(output.clone()));
        Ok(Some(output))
    }

    fn compile_build_script(
        &self,
        node: &PackageNode,
        script_path: &Path,
        build_dependencies: &[(String, BuiltLib)],
        request: &BuildRequest,
        executor: &Executor,
    ) -> Result<PathBuf> {
        let crate_name = format!(
            "build_script_{}",
            paths::normalize_crate_name(&node.manifest.package.name)
        );
        let output = request
            .out_dir
            .join(request.profile.as_str())
            .join("build")
            .join(&node.manifest.package.name)
            .join(paths::executable_name("build-script-build"));
        let dep_info = dep_info_path(
            request,
            &format!("build-script-{}", node.manifest.package.name),
        );

        let mut args = vec![
            "--crate-name".to_owned(),
            crate_name,
            "--edition".to_owned(),
            node.manifest.package.edition.clone(),
            paths::display_path(script_path),
            "-o".to_owned(),
            paths::display_path(&output),
            "--emit".to_owned(),
            format!("link,dep-info={}", paths::display_path(&dep_info)),
        ];
        add_feature_args(&node.features, &mut args);
        add_profile_args(request.profile, &mut args);
        add_metadata_args(
            &metadata_hash(node, request.profile, &self.host_triple(), "build-script")[..8],
            &mut args,
        );
        add_dependency_args(build_dependencies, &mut args);
        if let Some(linker) = &self.linker {
            args.push("-C".to_owned());
            args.push(format!("linker={}", paths::display_path(linker)));
        }

        let mut inputs = vec![
            node.manifest.path.clone(),
            paths::canonicalize(script_path)?,
        ];
        for (_, dependency) in build_dependencies {
            inputs.push(dependency.path.clone());
        }
        let empty_env = BTreeMap::new();
        let action = self.rust_action(RustActionSpec {
            id: format!("build-script-compile:{}", node.manifest.package.name),
            mnemonic: "RustBuildScriptCompile",
            args,
            inputs,
            outputs: vec![output.clone(), dep_info],
            workdir: &node.manifest.root,
            request,
            extra_env: &empty_env,
        })?;
        executor.run(&action)?;
        Ok(output)
    }

    fn execute_build_script(
        &self,
        node: &PackageNode,
        script_bin: &Path,
        request: &BuildRequest,
        executor: &Executor,
    ) -> Result<BuildScriptOutput> {
        let build_root = request
            .out_dir
            .join(request.profile.as_str())
            .join("build")
            .join(&node.manifest.package.name);
        let out_dir = build_root.join("out");
        fs::create_dir_all(&out_dir)
            .with_context(format!("failed to create OUT_DIR {}", out_dir.display()))?;
        let stdout = build_root.join("output");

        let mut env = BTreeMap::new();
        env.insert("LANG".to_owned(), "C".to_owned());
        env.insert("LC_ALL".to_owned(), "C".to_owned());
        env.insert(
            "TMPDIR".to_owned(),
            paths::display_path(&build_root.join("tmp")),
        );
        env.insert(
            "TMP".to_owned(),
            paths::display_path(&build_root.join("tmp")),
        );
        env.insert(
            "TEMP".to_owned(),
            paths::display_path(&build_root.join("tmp")),
        );
        env.insert("OUT_DIR".to_owned(), paths::display_path(&out_dir));
        env.insert(
            "CARGO_MANIFEST_DIR".to_owned(),
            paths::display_path(&node.manifest.root),
        );
        env.insert(
            "CARGO_PKG_NAME".to_owned(),
            node.manifest.package.name.clone(),
        );
        env.insert(
            "CARGO_PKG_VERSION".to_owned(),
            node.manifest.package.version.clone(),
        );
        env.insert("HOST".to_owned(), self.host_triple());
        env.insert("TARGET".to_owned(), self.host_triple());
        env.insert("PROFILE".to_owned(), request.profile.as_str().to_owned());
        env.insert(
            "OPT_LEVEL".to_owned(),
            opt_level(request.profile).to_owned(),
        );
        env.insert(
            "DEBUG".to_owned(),
            (request.profile == BuildProfile::Debug).to_string(),
        );
        env.insert("NUM_JOBS".to_owned(), "1".to_owned());
        env.insert("RUSTC".to_owned(), paths::display_path(&self.rustc));

        fs::create_dir_all(build_root.join("tmp")).with_context(format!(
            "failed to create build script tmp {}",
            build_root.join("tmp").display()
        ))?;

        let mut key_material = BTreeMap::new();
        key_material.insert("language".to_owned(), "rust".to_owned());
        key_material.insert("build_script".to_owned(), "run".to_owned());
        key_material.insert("profile".to_owned(), request.profile.as_str().to_owned());
        key_material.insert("host".to_owned(), self.host_triple());

        let action = Action {
            id: format!("build-script-run:{}", node.manifest.package.name),
            mnemonic: "RustBuildScriptRun".to_owned(),
            program: script_bin.to_path_buf(),
            args: Vec::new(),
            env_bundle: None,
            env,
            inputs: build_script_run_inputs(node, script_bin)?,
            outputs: vec![out_dir.clone(), stdout.clone()],
            workdir: node.manifest.root.clone(),
            key_material,
            stdout: Some(stdout.clone()),
        };
        executor.run(&action)?;

        let stdout_text = fs::read_to_string(&stdout).with_context(format!(
            "failed to read build script output {}",
            stdout.display()
        ))?;
        let mut parsed = parse_build_script_stdout(&stdout_text);
        for warning in &parsed.warnings {
            eprintln!("warning: {warning}");
        }
        if let Some(error) = parsed.errors.first() {
            return Err(TongError::unsupported(format!(
                "build script error: {error}"
            )));
        }
        parsed.out_dir = out_dir;
        parsed.stdout = stdout;
        parsed.generated_inputs = collect_files(&parsed.out_dir)?;
        for path in &parsed.rerun_if_changed {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                node.manifest.root.join(path)
            };
            if path.exists() {
                parsed.generated_inputs.push(paths::canonicalize(&path)?);
            }
        }
        Ok(parsed)
    }

    fn host_triple(&self) -> String {
        self.rustc_version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .unwrap_or("unknown-host")
            .to_owned()
    }

    fn rust_action(&self, spec: RustActionSpec<'_>) -> Result<Action> {
        let RustActionSpec {
            id,
            mnemonic,
            args,
            inputs,
            outputs,
            workdir,
            request,
            extra_env,
        } = spec;
        let tmp = request
            .out_dir
            .join("tmp")
            .join(hash::hash_bytes(id.as_bytes()));
        fs::create_dir_all(&tmp)
            .with_context(format!("failed to create action tmp {}", tmp.display()))?;

        let mut env = BTreeMap::new();
        env.insert("LANG".to_owned(), "C".to_owned());
        env.insert("LC_ALL".to_owned(), "C".to_owned());
        env.insert("TMPDIR".to_owned(), paths::display_path(&tmp));
        env.insert("TMP".to_owned(), paths::display_path(&tmp));
        env.insert("TEMP".to_owned(), paths::display_path(&tmp));
        env.extend(extra_env.clone());

        let mut key_material = BTreeMap::new();
        key_material.insert("language".to_owned(), "rust".to_owned());
        key_material.insert("rustc_version".to_owned(), self.rustc_version.clone());
        key_material.insert("profile".to_owned(), request.profile.as_str().to_owned());
        key_material.insert(
            "host".to_owned(),
            format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        );

        Ok(Action {
            id,
            mnemonic: mnemonic.to_owned(),
            program: self.rustc.clone(),
            args,
            env_bundle: EnvBundle::host_rust_toolchain(),
            env,
            inputs,
            outputs,
            workdir: workdir.to_path_buf(),
            key_material,
            stdout: None,
        })
    }
}

impl LanguageBackend<ProjectGraph> for RustBackend {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn build(&mut self, graph: &ProjectGraph, request: &BuildRequest) -> Result<BuildOutput> {
        self.build_root(graph, request)
    }
}

fn resolve_rustc() -> Result<PathBuf> {
    let candidate = if let Some(path) = std::env::var_os("RUSTC") {
        PathBuf::from(path)
    } else {
        paths::find_program_uncanonicalized("rustc")?
    };

    let output = Command::new(&candidate)
        .args(["--print", "sysroot"])
        .output()
        .with_context(format!(
            "failed to query rust sysroot using {}",
            candidate.display()
        ))?;

    if output.status.success() {
        let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !sysroot.is_empty() {
            let rustc_name = if cfg!(windows) { "rustc.exe" } else { "rustc" };
            let toolchain_rustc = PathBuf::from(sysroot).join("bin").join(rustc_name);
            if toolchain_rustc.exists() {
                return paths::canonicalize(&toolchain_rustc);
            }
        }
    }

    paths::canonicalize(&candidate)
}

fn package_inputs(node: &PackageNode) -> Result<Vec<PathBuf>> {
    let mut inputs = vec![node.manifest.path.clone()];
    inputs.extend(collect_package_files(&node.manifest.root)?);
    if let Some(build_script) = &node.manifest.build_script {
        inputs.push(paths::canonicalize(build_script)?);
    }
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

fn package_inputs_with_dep_info(node: &PackageNode, dep_info: &Path) -> Result<Vec<PathBuf>> {
    if dep_info.exists() {
        let raw = fs::read_to_string(dep_info)
            .with_context(format!("failed to read dep-info {}", dep_info.display()))?;
        let mut inputs = vec![node.manifest.path.clone()];
        for input in parse_makefile_dep_info(&raw) {
            let path = if input.is_absolute() {
                input
            } else {
                node.manifest.root.join(input)
            };
            if path.exists() {
                inputs.push(paths::canonicalize(&path)?);
            }
        }
        if let Some(build_script) = &node.manifest.build_script {
            inputs.push(paths::canonicalize(build_script)?);
        }
        inputs.sort();
        inputs.dedup();
        return Ok(inputs);
    }
    package_inputs(node)
}

fn dep_info_path(request: &BuildRequest, id: &str) -> PathBuf {
    request
        .out_dir
        .join(request.profile.as_str())
        .join("dep-info")
        .join(format!("{}.d", hash::hash_bytes(id.as_bytes())))
}

fn build_script_run_inputs(node: &PackageNode, script_bin: &Path) -> Result<Vec<PathBuf>> {
    let mut inputs = vec![script_bin.to_path_buf()];
    inputs.extend(collect_package_files(&node.manifest.root)?);
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

fn collect_package_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files_inner(root, &mut files, true)?;
    files.sort();
    Ok(files)
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    collect_files_inner(root, &mut files, false)?;
    files.sort();
    Ok(files)
}

fn collect_files_inner(root: &Path, files: &mut Vec<PathBuf>, skip_build_dirs: bool) -> Result<()> {
    for entry in fs::read_dir(root).with_context(format!("failed to read {}", root.display()))? {
        let entry = entry.with_context(format!("failed to read entry in {}", root.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            if skip_build_dirs && is_ignored_package_dir(&path) {
                continue;
            }
            collect_files_inner(&path, files, skip_build_dirs)?;
        } else if file_type.is_file() {
            files.push(paths::canonicalize(&path)?);
        }
    }
    Ok(())
}

fn metadata_hash(
    node: &PackageNode,
    profile: BuildProfile,
    host: &str,
    crate_kind: &str,
) -> String {
    let mut hasher = hash::StableHasher::new();
    hasher.update_str(&node.manifest.package.name);
    hasher.update_str(&node.manifest.package.version);
    hasher.update_str(&paths::display_path(&node.key.0));
    hasher.update_str(profile.as_str());
    hasher.update_str(host);
    hasher.update_str(crate_kind);
    for feature in &node.features {
        hasher.update_str(feature);
    }
    hasher.finish_hex()
}

fn target_enabled(required_features: &[String], enabled_features: &BTreeSet<String>) -> bool {
    required_features
        .iter()
        .all(|feature| enabled_features.contains(feature))
}

fn example_to_bin(example: &ExampleTarget) -> BinTarget {
    BinTarget {
        name: example.name.clone(),
        path: example.path.clone(),
        required_features: example.required_features.clone(),
    }
}

fn test_to_bin(test: &TestTarget) -> BinTarget {
    BinTarget {
        name: test.name.clone(),
        path: test.path.clone(),
        required_features: test.required_features.clone(),
    }
}

fn is_ignored_package_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("target" | ".git" | ".jj")
    )
}

fn rustc_version(rustc: &Path) -> Result<String> {
    let output = Command::new(rustc)
        .arg("-Vv")
        .output()
        .with_context(format!("failed to query {}", rustc.display()))?;
    if !output.status.success() {
        return Err(TongError::CommandFailed {
            program: rustc.to_path_buf(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn default_linker(rustc_version: &str) -> Option<PathBuf> {
    let host = rustc_version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or_default();

    if host.contains("msvc") {
        None
    } else {
        paths::find_program_uncanonicalized("cc").ok()
    }
}
