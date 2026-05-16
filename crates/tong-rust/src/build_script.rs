use std::collections::BTreeMap;
use std::path::PathBuf;
use tong_core::paths;

#[derive(Debug, Clone)]
pub(super) struct BuildScriptOutput {
    pub(super) out_dir: PathBuf,
    pub(super) stdout: PathBuf,
    pub(super) cfgs: Vec<String>,
    pub(super) rustc_env: BTreeMap<String, String>,
    pub(super) link_search: Vec<String>,
    pub(super) link_libs: Vec<String>,
    pub(super) link_args: Vec<String>,
    pub(super) cdylib_link_args: Vec<String>,
    pub(super) rerun_if_changed: Vec<PathBuf>,
    pub(super) rerun_if_env_changed: Vec<String>,
    pub(super) warnings: Vec<String>,
    pub(super) errors: Vec<String>,
    pub(super) generated_inputs: Vec<PathBuf>,
}

impl BuildScriptOutput {
    pub(super) fn empty() -> Self {
        Self {
            out_dir: PathBuf::new(),
            stdout: PathBuf::new(),
            cfgs: Vec::new(),
            rustc_env: BTreeMap::new(),
            link_search: Vec::new(),
            link_libs: Vec::new(),
            link_args: Vec::new(),
            cdylib_link_args: Vec::new(),
            rerun_if_changed: Vec::new(),
            rerun_if_env_changed: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
            generated_inputs: Vec::new(),
        }
    }
}

pub(super) fn parse_build_script_stdout(stdout: &str) -> BuildScriptOutput {
    let mut output = BuildScriptOutput::empty();
    for line in stdout.lines() {
        let Some(rest) = line
            .strip_prefix("cargo::")
            .or_else(|| line.strip_prefix("cargo:"))
        else {
            continue;
        };
        if let Some(cfg) = rest.strip_prefix("rustc-cfg=") {
            output.cfgs.push(cfg.to_owned());
        } else if let Some(env) = rest.strip_prefix("rustc-env=") {
            if let Some((key, value)) = env.split_once('=') {
                output.rustc_env.insert(key.to_owned(), value.to_owned());
            }
        } else if let Some(search) = rest.strip_prefix("rustc-link-search=") {
            output.link_search.push(search.to_owned());
        } else if let Some(lib) = rest.strip_prefix("rustc-link-lib=") {
            output.link_libs.push(lib.to_owned());
        } else if let Some(arg) = rest.strip_prefix("rustc-link-arg=") {
            output.link_args.push(arg.to_owned());
        } else if let Some(arg) = rest.strip_prefix("rustc-cdylib-link-arg=") {
            output.cdylib_link_args.push(arg.to_owned());
        } else if let Some(path) = rest.strip_prefix("rerun-if-changed=") {
            output.rerun_if_changed.push(PathBuf::from(path));
        } else if let Some(key) = rest.strip_prefix("rerun-if-env-changed=") {
            output.rerun_if_env_changed.push(key.to_owned());
        } else if let Some(warning) = rest.strip_prefix("warning=") {
            output.warnings.push(warning.to_owned());
        } else if let Some(error) = rest.strip_prefix("error=") {
            output.errors.push(error.to_owned());
        }
    }
    output
}

pub(super) fn build_script_env(
    build_script: Option<&BuildScriptOutput>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some(build_script) = build_script {
        env.insert(
            "OUT_DIR".to_owned(),
            paths::display_path(&build_script.out_dir),
        );
        for (key, value) in &build_script.rustc_env {
            env.insert(key.clone(), value.clone());
        }
    }
    env
}

pub(super) fn add_build_script_args(
    build_script: Option<&BuildScriptOutput>,
    args: &mut Vec<String>,
) {
    let Some(build_script) = build_script else {
        return;
    };

    for cfg in &build_script.cfgs {
        args.push("--cfg".to_owned());
        args.push(cfg.clone());
    }
    for search in &build_script.link_search {
        args.push("-L".to_owned());
        args.push(search.clone());
    }
    for lib in &build_script.link_libs {
        args.push("-l".to_owned());
        args.push(lib.clone());
    }
    for arg in &build_script.link_args {
        args.push("-C".to_owned());
        args.push(format!("link-arg={arg}"));
    }
    for arg in &build_script.cdylib_link_args {
        args.push("-C".to_owned());
        args.push(format!("link-arg={arg}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_build_script_stdout_directives() {
        let output = parse_build_script_stdout(
            "\
cargo:rustc-cfg=has_demo
cargo::rustc-cfg=has_double_colon
cargo:rustc-env=DEMO=value
cargo:rustc-link-search=native=/tmp/lib
cargo:rustc-link-lib=static=demo
cargo:rustc-link-arg=-Wl,--as-needed
cargo:rerun-if-changed=build-data.txt
cargo::rerun-if-env-changed=DEMO_ENV
cargo:warning=careful
ignored line
",
        );

        assert_eq!(output.cfgs, ["has_demo", "has_double_colon"]);
        assert_eq!(output.rustc_env["DEMO"], "value");
        assert_eq!(output.link_search, ["native=/tmp/lib"]);
        assert_eq!(output.link_libs, ["static=demo"]);
        assert_eq!(output.link_args, ["-Wl,--as-needed"]);
        assert_eq!(output.rerun_if_changed, [PathBuf::from("build-data.txt")]);
        assert_eq!(output.rerun_if_env_changed, ["DEMO_ENV"]);
        assert_eq!(output.warnings, ["careful"]);
    }
}
