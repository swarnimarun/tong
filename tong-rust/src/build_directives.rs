//! Build-script directive parsing.
//!
//! Build scripts communicate with the compiler through `cargo:` lines on
//! stdout (PLAN.md section 8.6, strict declarative mode). Both the legacy
//! `cargo:` and namespaced `cargo::` spellings are accepted for the known
//! directives. Any other `cargo::key=value` line is a script failure
//! (Cargo errors too); a legacy `cargo:KEY=VALUE` line whose key is not a
//! known directive is the legacy metadata form — the pair is exposed as an
//! environment variable to the package's dependents. The namespaced form is
//! `cargo::metadata=KEY=VALUE`.

/// A parsed set of build-script directives.
#[derive(Clone, Debug, Default)]
pub struct Directives {
    /// `cargo:rustc-cfg=...` — cfg flags.
    pub cfgs: Vec<String>,
    /// `cargo:rustc-env=K=V` — environment for this package's compilation.
    pub env: Vec<(String, String)>,
    /// `cargo::metadata=K=V` or legacy `cargo:K=V` — metadata exported to
    /// direct dependents when this package declares `links`.
    pub metadata: Vec<(String, String)>,
    /// `cargo:rustc-link-lib=...` — native libraries, with an optional
    /// kind prefix (`static:`, `dylib:`, `framework:`).
    pub link_libs: Vec<(Option<String>, String)>,
    /// `cargo:rustc-link-search=...` — native search paths.
    pub link_search: Vec<String>,
    /// `cargo:rustc-flags=...` — raw flags (whitespace-split).
    pub raw_flags: Vec<String>,
    /// `cargo:rustc-link-arg=...` — link args for every consuming target.
    pub link_args: Vec<String>,
    /// `cargo:rustc-link-arg-bins=...` — link args for binary targets.
    pub link_arg_bins: Vec<String>,
    /// `cargo:rustc-link-arg-tests=...` — link args for test targets.
    pub link_arg_tests: Vec<String>,
    /// `cargo:rustc-link-arg-examples=...` — link args for example targets.
    pub link_arg_examples: Vec<String>,
    /// `cargo:rustc-cdylib-link-arg=...` — link args for cdylib libraries.
    pub cdylib_link_args: Vec<String>,
    /// `cargo:rustc-metadata=...` — extra metadata appended to the crate
    /// metadata hash.
    pub extra_metadata: Vec<String>,
    /// `cargo:rustc-check-cfg=...` — check-cfg declarations.
    pub check_cfgs: Vec<String>,
    /// `cargo:rerun-if-changed=...` — paths that narrow the build-script
    /// run action's inputs (resolved against the package dir).
    pub rerun_if_changed: Vec<String>,
    /// `cargo:rerun-if-env-changed=...` — environment variables that
    /// trigger a rerun when their values change.
    pub rerun_if_env_changed: Vec<String>,
    /// `cargo:warning=...` — surfaced to the user.
    pub warnings: Vec<String>,
    /// `cargo:error=...` — build failure.
    pub errors: Vec<String>,
}

/// Parses build-script stdout (which may also contain compiler diagnostics
/// or program output; only `cargo:` lines are interpreted).
pub fn parse_directives(stdout: &str) -> Directives {
    let mut out = Directives::default();
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        let namespaced = trimmed.starts_with("cargo::");
        let Some(body) = trimmed
            .strip_prefix("cargo::")
            .or_else(|| trimmed.strip_prefix("cargo:"))
        else {
            continue;
        };
        let Some((key, value)) = body.split_once('=') else {
            // Not a key=value directive (e.g. `cargo:foo`); ignore.
            continue;
        };
        match key {
            "rustc-cfg" => out.cfgs.push(unquote(value)),
            "rustc-env" => {
                if let Some((key, value)) = value.split_once('=') {
                    out.env.push((key.to_owned(), value.to_owned()));
                }
            }
            "metadata" => {
                if let Some((key, value)) = value.split_once('=') {
                    out.metadata.push((key.to_owned(), value.to_owned()));
                } else {
                    out.errors.push(
                        "build-script directive `cargo::metadata` requires `KEY=VALUE`".to_owned(),
                    );
                }
            }
            "rustc-link-lib" => out.link_libs.push(parse_link_lib(value)),
            "rustc-link-search" => out.link_search.push(value.to_owned()),
            "rustc-flags" => {
                out.raw_flags
                    .extend(value.split_whitespace().map(str::to_owned));
            }
            "rustc-link-arg" => out.link_args.push(value.to_owned()),
            "rustc-link-arg-bins" => out.link_arg_bins.push(value.to_owned()),
            "rustc-link-arg-tests" => out.link_arg_tests.push(value.to_owned()),
            "rustc-link-arg-examples" => out.link_arg_examples.push(value.to_owned()),
            "rustc-cdylib-link-arg" | "rustc-link-arg-cdylib" => {
                out.cdylib_link_args.push(value.to_owned());
            }
            "rustc-metadata" => out.extra_metadata.push(value.to_owned()),
            "rustc-check-cfg" => out.check_cfgs.push(value.to_owned()),
            "rerun-if-changed" => out.rerun_if_changed.push(value.to_owned()),
            "rerun-if-env-changed" => out.rerun_if_env_changed.push(value.to_owned()),
            "warning" => out.warnings.push(value.to_owned()),
            "error" => out.errors.push(value.to_owned()),
            other if namespaced => {
                // Unknown namespaced directive: script failure (Cargo
                // errors on unknown directives too).
                out.errors.push(format!(
                    "unsupported build-script directive `cargo::{other}`"
                ));
            }
            _ => {
                // Legacy `cargo:KEY=VALUE` with an unknown key is the
                // metadata form: an env pair for dependents.
                out.metadata.push((key.to_owned(), value.to_owned()));
            }
        }
    }
    out
}

/// Splits an optional link-lib kind prefix: `static:foo` → `(static, foo)`.
fn parse_link_lib(value: &str) -> (Option<String>, String) {
    for kind in ["static", "dylib", "framework"] {
        if let Some(rest) = value.strip_prefix(&format!("{kind}:")) {
            return (Some(kind.to_owned()), rest.to_owned());
        }
    }
    (None, value.to_owned())
}

/// Strips quotes from a `--cfg` value (`"..."` or `'...'`).
fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        trimmed[1..trimmed.len() - 1].to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_supported_directives() {
        let directives = parse_directives(
            "cargo:rustc-cfg=feature=\"x\"\n\
             cargo::rustc-env=KEY=value\n\
             cargo:rustc-link-lib=static:foo\n\
             cargo:rustc-link-lib=bar\n\
             cargo:rustc-link-search=native=/usr/lib\n\
             cargo:rustc-flags=-C target-cpu=native\n\
             cargo::rustc-link-arg=-Wl,--as-needed\n\
             cargo::rustc-link-arg-bins=-static\n\
             cargo::rustc-link-arg-tests=-T\n\
             cargo::rustc-link-arg-examples=-E\n\
             cargo::rustc-cdylib-link-arg=-Wl,-install_name\n\
             cargo::rustc-metadata=abc\n\
             cargo::metadata=DEP_KEY=dep-value\n\
             cargo::rustc-check-cfg=cfg(feature, values(\"a\"))\n\
             cargo:rerun-if-changed=build.rs\n\
             cargo:rerun-if-env-changed=CARGO_FOO\n\
             cargo:warning=look out\n\
             cargo:error=boom\n",
        );
        assert_eq!(directives.cfgs, vec!["feature=\"x\""]);
        assert_eq!(directives.env, vec![("KEY".to_owned(), "value".to_owned())]);
        assert_eq!(
            directives.metadata,
            vec![("DEP_KEY".to_owned(), "dep-value".to_owned())]
        );
        assert_eq!(
            directives.link_libs,
            vec![
                (Some("static".to_owned()), "foo".to_owned()),
                (None, "bar".to_owned()),
            ]
        );
        assert_eq!(directives.link_search, vec!["native=/usr/lib"]);
        assert_eq!(directives.raw_flags, vec!["-C", "target-cpu=native"]);
        assert_eq!(directives.link_args, vec!["-Wl,--as-needed"]);
        assert_eq!(directives.link_arg_bins, vec!["-static"]);
        assert_eq!(directives.link_arg_tests, vec!["-T"]);
        assert_eq!(directives.link_arg_examples, vec!["-E"]);
        assert_eq!(directives.cdylib_link_args, vec!["-Wl,-install_name"]);
        assert_eq!(directives.extra_metadata, vec!["abc"]);
        assert_eq!(directives.check_cfgs, vec!["cfg(feature, values(\"a\"))"]);
        assert_eq!(directives.rerun_if_changed, vec!["build.rs"]);
        assert_eq!(directives.rerun_if_env_changed, vec!["CARGO_FOO"]);
        assert_eq!(directives.warnings, vec!["look out"]);
        assert_eq!(directives.errors, vec!["boom"]);
    }

    #[test]
    fn metadata_keys_become_dependent_env() {
        let directives = parse_directives(
            "cargo:FOO=bar\ncargo:BAZ=qux\ncargo::metadata=MODERN=value=with=equals\n",
        );
        assert_eq!(
            directives.metadata,
            vec![
                ("FOO".to_owned(), "bar".to_owned()),
                ("BAZ".to_owned(), "qux".to_owned()),
                ("MODERN".to_owned(), "value=with=equals".to_owned()),
            ]
        );
        assert!(directives.env.is_empty());
        assert!(directives.errors.is_empty());
    }

    #[test]
    fn malformed_namespaced_metadata_fails_the_script() {
        let directives = parse_directives("cargo::metadata=missing-value\n");
        assert_eq!(directives.errors.len(), 1);
        assert!(directives.errors[0].contains("KEY=VALUE"));
    }

    #[test]
    fn unknown_namespaced_directives_fail_the_script() {
        let directives = parse_directives("cargo::not-a-thing=value\n");
        assert_eq!(directives.errors.len(), 1);
        assert!(directives.errors[0].contains("not-a-thing"));
    }

    #[test]
    fn plain_output_lines_are_ignored() {
        let directives = parse_directives("hello world\nsome warning text\n");
        assert_eq!(directives.errors.len(), 0);
        assert!(directives.cfgs.is_empty());
    }
}
