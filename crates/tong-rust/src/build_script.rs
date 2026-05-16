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
ignored line
",
        );

        assert_eq!(output.cfgs, ["has_demo", "has_double_colon"]);
        assert_eq!(output.rustc_env["DEMO"], "value");
        assert_eq!(output.link_search, ["native=/tmp/lib"]);
        assert_eq!(output.link_libs, ["static=demo"]);
    }
}
