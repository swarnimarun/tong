use std::collections::BTreeSet;
use std::path::PathBuf;
use tong_core::language::BuildProfile;
use tong_core::paths;

#[derive(Debug, Clone)]
pub(super) struct BuiltLib {
    pub(super) extern_name: String,
    pub(super) path: PathBuf,
}

pub(super) fn rust_lib_output_name(crate_name: &str, hash: &str, proc_macro: bool) -> String {
    if proc_macro {
        format!(
            "{}{}-{}.{}",
            std::env::consts::DLL_PREFIX,
            crate_name,
            hash,
            std::env::consts::DLL_EXTENSION
        )
    } else {
        format!("lib{crate_name}-{hash}.rlib")
    }
}

pub(super) fn add_feature_args(features: &BTreeSet<String>, args: &mut Vec<String>) {
    for feature in features {
        args.push("--cfg".to_owned());
        args.push(format!("feature={feature:?}"));
    }
}

pub(super) fn add_profile_args(profile: BuildProfile, args: &mut Vec<String>) {
    match profile {
        BuildProfile::Debug => {
            args.push("-C".to_owned());
            args.push("debuginfo=2".to_owned());
        }
        BuildProfile::Release => {
            args.push("-C".to_owned());
            args.push("opt-level=3".to_owned());
            args.push("-C".to_owned());
            args.push("debug-assertions=no".to_owned());
        }
    }
}

pub(super) fn add_metadata_hash(metadata_hash: &str, args: &mut Vec<String>) {
    args.push("-C".to_owned());
    args.push(format!("metadata={metadata_hash}"));
}

pub(super) fn opt_level(profile: BuildProfile) -> &'static str {
    match profile {
        BuildProfile::Debug => "0",
        BuildProfile::Release => "3",
    }
}

pub(super) fn add_dependency_args(dependencies: &[(String, BuiltLib)], args: &mut Vec<String>) {
    if dependencies.is_empty() {
        return;
    }

    let mut seen_dirs = BTreeSet::new();
    for (_, dependency) in dependencies {
        if let Some(parent) = dependency.path.parent()
            && seen_dirs.insert(parent.to_path_buf())
        {
            args.push("-L".to_owned());
            args.push(format!("dependency={}", paths::display_path(parent)));
        }
    }

    for (alias, dependency) in dependencies {
        args.push("--extern".to_owned());
        args.push(format!(
            "{}={}",
            paths::normalize_crate_name(alias),
            paths::display_path(&dependency.path)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_lib_output_names_match_crate_kind() {
        assert_eq!(
            rust_lib_output_name("demo", "abcdef12", false),
            "libdemo-abcdef12.rlib"
        );

        let proc_macro = rust_lib_output_name("demo", "abcdef12", true);
        assert!(proc_macro.starts_with(&format!("{}demo-abcdef12.", std::env::consts::DLL_PREFIX)));
        assert!(proc_macro.ends_with(std::env::consts::DLL_EXTENSION));
        assert_ne!(proc_macro, "libdemo-abcdef12.rlib");
    }

    #[test]
    fn dependency_args_deduplicate_search_dirs_and_normalize_aliases() {
        let deps = vec![
            (
                "foo-bar".to_owned(),
                BuiltLib {
                    extern_name: "foo_bar".to_owned(),
                    path: PathBuf::from("/tmp/deps/libfoo.rlib"),
                },
            ),
            (
                "baz".to_owned(),
                BuiltLib {
                    extern_name: "baz".to_owned(),
                    path: PathBuf::from("/tmp/deps/libbaz.rlib"),
                },
            ),
        ];
        let mut args = Vec::new();

        add_dependency_args(&deps, &mut args);

        let search_count = args.iter().filter(|arg| arg.as_str() == "-L").count();
        assert_eq!(search_count, 1);
        assert!(args.contains(&"foo_bar=/tmp/deps/libfoo.rlib".to_owned()));
        assert!(args.contains(&"baz=/tmp/deps/libbaz.rlib".to_owned()));
    }

    #[test]
    fn profile_args_are_small_and_predictable() {
        let mut debug = Vec::new();
        add_profile_args(BuildProfile::Debug, &mut debug);
        assert_eq!(debug, ["-C", "debuginfo=2"]);

        let mut release = Vec::new();
        add_profile_args(BuildProfile::Release, &mut release);
        assert_eq!(release, ["-C", "opt-level=3", "-C", "debug-assertions=no"]);
        assert_eq!(opt_level(BuildProfile::Release), "3");
    }
}
