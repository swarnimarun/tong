//! Build-script directive parsing.
//!
//! Build scripts communicate with the compiler through `cargo:` lines on
//! stdout (PLAN.md section 8.6, strict declarative mode). Both the `cargo:`
//! and `cargo::` spellings are accepted. Directives Tong cannot represent
//! faithfully are ignored rather than silently misapplied, matching the
//! plan's rule that unsupported behavior must not change semantics.

/// A parsed set of build-script directives.
#[derive(Clone, Debug, Default)]
pub struct Directives {
    /// `cargo:rustc-cfg=...` — cfg flags.
    pub cfgs: Vec<String>,
    /// `cargo:rustc-env=K=V` — environment for dependents.
    pub env: Vec<(String, String)>,
    /// `cargo:rustc-link-lib=...` — native libraries.
    pub link_libs: Vec<String>,
    /// `cargo:rustc-link-search=...` — native search paths.
    pub link_search: Vec<String>,
    /// `cargo:rustc-flags=...` — raw flags (whitespace-split).
    pub raw_flags: Vec<String>,
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
        let Some(body) = line
            .trim_start()
            .strip_prefix("cargo::")
            .or_else(|| line.trim_start().strip_prefix("cargo:"))
        else {
            continue;
        };
        match body.split_once('=') {
            Some(("rustc-cfg", value)) => out.cfgs.push(unquote(value)),
            Some(("rustc-env", value)) => {
                if let Some((key, value)) = value.split_once('=') {
                    out.env.push((key.to_owned(), value.to_owned()));
                }
            }
            Some(("rustc-link-lib", value)) => out.link_libs.push(value.to_owned()),
            Some(("rustc-link-search", value)) => out.link_search.push(value.to_owned()),
            Some(("rustc-flags", value)) => {
                out.raw_flags
                    .extend(value.split_whitespace().map(str::to_owned));
            }
            Some(("warning", value)) => out.warnings.push(value.to_owned()),
            Some(("error", value)) => out.errors.push(value.to_owned()),
            // rerun-if-* is subsumed by whole-package source inputs
            // (PLAN.md section 8.3); unknown metadata keys are ignored.
            _ => {}
        }
    }
    out
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
    fn parses_common_directives() {
        let text = "\
compiler output line
cargo:rustc-cfg=feature=\"foo\"
cargo::rustc-env=KEY=value
cargo:rustc-link-lib=dylib=SDL3.0
cargo:rustc-link-search=native=/x/y
cargo:rustc-flags=-L native=/a -l z
cargo:warning=be careful
cargo:rerun-if-changed=build.rs
cargo:custom-metadata=ignored
";
        let d = parse_directives(text);
        assert_eq!(d.cfgs, vec!["feature=\"foo\""]);
        assert_eq!(d.env, vec![("KEY".to_owned(), "value".to_owned())]);
        assert_eq!(d.link_libs, vec!["dylib=SDL3.0"]);
        assert_eq!(d.link_search, vec!["native=/x/y"]);
        assert_eq!(d.raw_flags, vec!["-L", "native=/a", "-l", "z"]);
        assert_eq!(d.warnings, vec!["be careful"]);
        assert!(d.errors.is_empty());
    }

    #[test]
    fn reports_build_errors() {
        let d = parse_directives("cargo:error=bindgen failed\n");
        assert_eq!(d.errors, vec!["bindgen failed"]);
    }
}
