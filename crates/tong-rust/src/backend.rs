use crate::args::{
    BuiltLib, add_dependency_args, add_feature_args, add_profile_args, opt_level,
    rust_lib_output_name,
};
use crate::build_script::{
    BuildScriptOutput, add_build_script_args, build_script_env, parse_build_script_stdout,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tong_core::action::Action;
use tong_core::cache::ActionCache;
use tong_core::error::{IoContext, Result, TongError};
use tong_core::exec::Executor;
use tong_core::hash;
use tong_core::language::{BuildOutput, BuildProfile, BuildRequest, LanguageBackend};
use tong_core::paths;
use tong_graph::{PackageKey, PackageNode, ProjectGraph};
use tong_manifest::{BinTarget, LibTarget};

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
        let linker = paths::find_program("cc").ok();
        let rustc_version = rustc_version(&rustc)?;
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
            verbose: request.verbose,
        };

        let mut artifacts = Vec::new();
        let root = graph.package(&graph.root)?;

        let root_lib = self.build_package_lib(graph, &graph.root, request, &executor)?;
        if let Some(lib) = &root_lib {
            artifacts.push(lib.path.clone());
        }

        for bin in &root.manifest.bins {
            let path = self.build_bin(graph, root, bin, root_lib.as_ref(), request, &executor)?;
            artifacts.push(path);
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

        let build_script = self.run_build_script(node, request, executor)?;
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
        let output = request
            .out_dir
            .join(request.profile.as_str())
            .join("deps")
            .join(rust_lib_output_name(
                &crate_name,
                short_hash,
                lib.proc_macro,
            ));
        let crate_type = if lib.proc_macro { "proc-macro" } else { "lib" };

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
        ];
        add_feature_args(&node.features, &mut args);
        add_profile_args(request.profile, &mut args);
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

        let mut inputs = package_inputs(node)?;
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
            outputs: vec![output.clone()],
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

        let build_script = self.run_build_script(node, request, executor)?;

        let crate_name = paths::normalize_crate_name(&bin.name);
        let output = request
            .out_dir
            .join(request.profile.as_str())
            .join("bin")
            .join(paths::executable_name(&bin.name));

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
        ];
        add_feature_args(&node.features, &mut args);
        add_profile_args(request.profile, &mut args);
        add_dependency_args(&dependencies, &mut args);
        add_build_script_args(build_script.as_ref(), &mut args);

        if let Some(linker) = &self.linker {
            args.push("-C".to_owned());
            args.push(format!("linker={}", paths::display_path(linker)));
        }

        let mut inputs = package_inputs(node)?;
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
            outputs: vec![output.clone()],
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

        let script_bin = self.compile_build_script(node, script_path, request, executor)?;
        let output = self.execute_build_script(node, &script_bin, request, executor)?;
        self.build_scripts
            .insert(node.key.clone(), Some(output.clone()));
        Ok(Some(output))
    }

    fn compile_build_script(
        &self,
        node: &PackageNode,
        script_path: &Path,
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

        let mut args = vec![
            "--crate-name".to_owned(),
            crate_name,
            "--edition".to_owned(),
            node.manifest.package.edition.clone(),
            paths::display_path(script_path),
            "-o".to_owned(),
            paths::display_path(&output),
        ];
        add_feature_args(&node.features, &mut args);
        add_profile_args(request.profile, &mut args);
        if let Some(linker) = &self.linker {
            args.push("-C".to_owned());
            args.push(format!("linker={}", paths::display_path(linker)));
        }

        let inputs = vec![
            node.manifest.path.clone(),
            paths::canonicalize(script_path)?,
        ];
        let empty_env = BTreeMap::new();
        let action = self.rust_action(RustActionSpec {
            id: format!("build-script-compile:{}", node.manifest.package.name),
            mnemonic: "RustBuildScriptCompile",
            args,
            inputs,
            outputs: vec![output.clone()],
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
        parsed.out_dir = out_dir;
        parsed.stdout = stdout;
        parsed.generated_inputs = collect_files(&parsed.out_dir)?;
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
