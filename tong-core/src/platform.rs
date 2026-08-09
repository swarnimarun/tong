//! Platform keys: canonical constraint sets.
//!
//! A platform is a set of constraints such as `os = "linux"`,
//! `arch = "x86_64"`, `abi = "gnu"` (PLAN.md section 7). Tong distinguishes
//! host, execution, and target platforms; this type identifies a platform in
//! a canonical, hashable form. Well-known constraint keys are not
//! special-cased here — matching and toolchain resolution live in
//! `tong-graph`.

use std::collections::BTreeMap;

use crate::canonical::{CanonicalEncode, Encoder};

/// A canonical set of platform constraints.
///
/// Constraints are stored in a sorted map, so the canonical encoding (and
/// therefore the digest) is independent of construction order.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PlatformKey {
    constraints: BTreeMap<String, String>,
}

impl PlatformKey {
    /// Creates a platform key from a constraint map.
    pub fn new(constraints: BTreeMap<String, String>) -> Self {
        Self { constraints }
    }

    /// Creates a platform key from constraint pairs.
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        let constraints = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        Self { constraints }
    }

    /// Returns the value of a constraint, if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.constraints.get(key).map(String::as_str)
    }

    /// Returns the constraint map.
    pub fn constraints(&self) -> &BTreeMap<String, String> {
        &self.constraints
    }
}

impl CanonicalEncode for PlatformKey {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_map(&self.constraints);
    }
}

/// Target-triple facts used to evaluate `cfg(...)` expressions.
#[derive(Clone, Debug, Default)]
pub struct TargetFacts {
    /// `target_arch` value (e.g. `x86_64`).
    pub arch: String,
    /// `target_os` value (e.g. `macos`, `linux`, `windows`).
    pub os: String,
    /// `target_family` value (`unix` or `windows`).
    pub family: String,
    /// `target_env` value (`gnu`, `msvc`, or empty).
    pub env: String,
    /// `target_vendor` value (e.g. `apple`, `unknown`, `pc`).
    pub vendor: String,
    /// `target_pointer_width` value (`64` or `32`).
    pub pointer_width: String,
}

/// Parses a rustc-style target triple into [`TargetFacts`]. Supported
/// triples: `x86_64-apple-darwin`, `aarch64-apple-darwin`,
/// `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
/// `aarch64-pc-windows-msvc`; anything else is [`CfgError::UnsupportedTriple`].
pub fn parse_triple(triple: &str) -> Result<TargetFacts, CfgError> {
    let mut parts = triple.split('-');
    let arch = parts.next().unwrap_or("");
    let vendor = parts.next().unwrap_or("");
    let os = parts.next().unwrap_or("");
    let env = parts.next().unwrap_or("");
    let (arch, os, family, env, vendor) = match (arch, vendor, os) {
        ("x86_64" | "aarch64", "apple", "darwin") => (
            arch.to_owned(),
            "macos".to_owned(),
            "unix".to_owned(),
            String::new(),
            "apple".to_owned(),
        ),
        ("x86_64" | "aarch64", "unknown", "linux") => {
            let env = if env.is_empty() { "gnu" } else { env };
            (
                arch.to_owned(),
                "linux".to_owned(),
                "unix".to_owned(),
                env.to_owned(),
                "unknown".to_owned(),
            )
        }
        ("x86_64" | "aarch64", "pc", "windows") => {
            let env = if env.is_empty() { "msvc" } else { env };
            (
                arch.to_owned(),
                "windows".to_owned(),
                "windows".to_owned(),
                env.to_owned(),
                "pc".to_owned(),
            )
        }
        _ => return Err(CfgError::UnsupportedTriple(triple.to_owned())),
    };
    let pointer_width = if arch == "x86_64" || arch == "aarch64" {
        "64".to_owned()
    } else {
        "32".to_owned()
    };
    Ok(TargetFacts {
        arch,
        os,
        family,
        env,
        vendor,
        pointer_width,
    })
}

/// Evaluates a Cargo `cfg(...)` expression against a host triple.
///
/// Supports `all(...)`, `any(...)`, `not(...)`, and the predicates
/// `target_os`, `target_arch`, `target_family`, `target_env`,
/// `target_vendor`, `unix`, and `windows`. The expression may be wrapped in
/// `cfg(...)` (as in `[target.'cfg(unix)'.dependencies]`).
/// Unknown predicates evaluate to `false` — custom cfgs (e.g. tokio's
/// `cfg(loom)`) are never set by the compiler, exactly as Cargo treats
/// them. `feature = "..."` is an error: targets never see feature cfgs,
/// and Cargo rejects them in target tables too.
pub fn eval_cfg(expr: &str, triple: &str) -> Result<bool, CfgError> {
    let facts = parse_triple(triple)?;
    let text = expr.trim();
    let text = text
        .strip_prefix("cfg(")
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(text);
    let mut tokens = Tokenizer::new(text);
    let parsed = parse_expr(&mut tokens)?;
    tokens.expect_end()?;
    eval_expr(&parsed, &facts)
}

/// Parsed `cfg` expression node.
enum CfgExpr {
    Predicate(String, Option<String>),
    All(Vec<CfgExpr>),
    Any(Vec<CfgExpr>),
    Not(Box<CfgExpr>),
}

/// `cfg` expression parsing or evaluation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CfgError {
    /// The triple is not one of the supported host triples.
    UnsupportedTriple(String),
    /// `feature = "..."` (targets never see feature cfgs; Cargo rejects
    /// them in target tables too).
    Unsupported(String),
    /// Malformed expression.
    Malformed(String),
}

impl std::fmt::Display for CfgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedTriple(triple) => {
                write!(f, "unsupported target triple {triple:?}")
            }
            Self::Unsupported(what) => write!(f, "unsupported cfg expression: {what}"),
            Self::Malformed(what) => write!(f, "malformed cfg expression: {what}"),
        }
    }
}

impl std::error::Error for CfgError {}

struct Tokenizer<'a> {
    rest: &'a str,
}

impl<'a> Tokenizer<'a> {
    fn new(text: &'a str) -> Self {
        Self { rest: text }
    }

    fn skip_ws(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_ws();
        self.rest.chars().next()
    }

    fn eat(&mut self, expected: char) -> Result<(), CfgError> {
        self.skip_ws();
        match self.rest.chars().next() {
            Some(c) if c == expected => {
                self.rest = &self.rest[c.len_utf8()..];
                Ok(())
            }
            other => Err(CfgError::Malformed(format!(
                "expected {expected:?}, found {other:?}"
            ))),
        }
    }

    /// Reads an identifier (`[a-zA-Z_][a-zA-Z0-9_]*`).
    fn ident(&mut self) -> Result<&'a str, CfgError> {
        self.skip_ws();
        let end = self
            .rest
            .char_indices()
            .find(|(index, c)| *index > 0 && !c.is_ascii_alphanumeric() && *c != '_')
            .map(|(index, _)| index)
            .unwrap_or(self.rest.len());
        let ident = &self.rest[..end];
        if ident.is_empty()
            || !(ident.chars().next().unwrap().is_ascii_alphabetic() || ident.starts_with('_'))
        {
            return Err(CfgError::Malformed(format!(
                "expected identifier, found {ident:?}"
            )));
        }
        self.rest = &self.rest[end..];
        Ok(ident)
    }

    /// Reads a double-quoted string.
    fn string(&mut self) -> Result<String, CfgError> {
        self.skip_ws();
        let rest = self.rest;
        let Some(stripped) = rest.strip_prefix('"') else {
            return Err(CfgError::Malformed(format!(
                "expected string literal, found {rest:?}"
            )));
        };
        let end = stripped
            .find('"')
            .ok_or_else(|| CfgError::Malformed("unterminated string literal".to_owned()))?;
        let value = stripped[..end].to_owned();
        self.rest = &stripped[end + 1..];
        Ok(value)
    }

    fn expect_end(&mut self) -> Result<(), CfgError> {
        self.skip_ws();
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(CfgError::Malformed(format!(
                "unexpected trailing input {:?}",
                self.rest
            )))
        }
    }
}

fn parse_expr(tokens: &mut Tokenizer<'_>) -> Result<CfgExpr, CfgError> {
    let name = tokens.ident()?;
    if tokens.peek() == Some('=') {
        tokens.eat('=')?;
        let value = tokens.string()?;
        return Ok(CfgExpr::Predicate(name.to_owned(), Some(value)));
    }
    match name {
        "all" | "any" => {
            tokens.eat('(')?;
            let mut items = Vec::new();
            // Empty argument lists are legal (e.g. `cfg(any())` appears in
            // generated manifests): `all()` is true, `any()` is false.
            if tokens.peek() != Some(')') {
                loop {
                    items.push(parse_expr(tokens)?);
                    if tokens.peek() == Some(',') {
                        tokens.eat(',')?;
                    } else {
                        break;
                    }
                }
            }
            tokens.eat(')')?;
            Ok(if name == "all" {
                CfgExpr::All(items)
            } else {
                CfgExpr::Any(items)
            })
        }
        "not" => {
            tokens.eat('(')?;
            let inner = parse_expr(tokens)?;
            tokens.eat(')')?;
            Ok(CfgExpr::Not(Box::new(inner)))
        }
        _ => Ok(CfgExpr::Predicate(name.to_owned(), None)),
    }
}

fn eval_expr(expr: &CfgExpr, facts: &TargetFacts) -> Result<bool, CfgError> {
    match expr {
        CfgExpr::All(items) => {
            for item in items {
                if !eval_expr(item, facts)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        CfgExpr::Any(items) => {
            for item in items {
                if eval_expr(item, facts)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        CfgExpr::Not(inner) => Ok(!eval_expr(inner, facts)?),
        CfgExpr::Predicate(name, value) => eval_predicate(name, value.as_deref(), facts),
    }
}

fn eval_predicate(name: &str, value: Option<&str>, facts: &TargetFacts) -> Result<bool, CfgError> {
    if let Some(expected) = value {
        let key = match name {
            "target_os" => Some(&facts.os),
            "target_arch" => Some(&facts.arch),
            "target_env" => Some(&facts.env),
            "target_vendor" => Some(&facts.vendor),
            "target_pointer_width" => Some(&facts.pointer_width),
            "target_family" | "unix" | "windows" => Some(&facts.family),
            "feature" => {
                return Err(CfgError::Unsupported(format!(
                    "feature = {expected:?}; feature cfgs never apply to \
                     target-specific deps"
                )));
            }
            // Unknown predicates are never set by the compiler: the
            // target never matches (cargo semantics — tokio's
            // `cfg(loom)` table is dropped, not an error).
            other => {
                let _ = other;
                return Ok(false);
            }
        };
        return Ok(key.map(String::as_str) == Some(expected));
    }
    // Bare predicates are only meaningful for families.
    match name {
        "unix" | "windows" | "target_family" => Ok(facts.family == name),
        "feature" => Err(CfgError::Unsupported(format!(
            "feature = {:?}; feature cfgs never apply to target-specific deps",
            value.unwrap_or("")
        ))),
        // A bare custom predicate (e.g. `loom`) is never set: false.
        other => {
            let _ = other;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical;

    #[test]
    fn digest_is_order_independent() {
        let a = PlatformKey::from_pairs(&[("os", "linux"), ("arch", "x86_64"), ("abi", "gnu")]);
        let b = PlatformKey::from_pairs(&[("abi", "gnu"), ("arch", "x86_64"), ("os", "linux")]);
        assert_eq!(canonical::digest_of(&a), canonical::digest_of(&b));
    }

    #[test]
    fn parses_known_triples() {
        let facts = parse_triple("aarch64-apple-darwin").unwrap();
        assert_eq!(facts.os, "macos");
        assert_eq!(facts.family, "unix");
        assert_eq!(facts.vendor, "apple");
        assert_eq!(facts.env, "");
        let facts = parse_triple("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(facts.os, "linux");
        assert_eq!(facts.env, "gnu");
        let facts = parse_triple("x86_64-pc-windows-msvc").unwrap();
        assert_eq!(facts.os, "windows");
        assert_eq!(facts.family, "windows");
        assert_eq!(facts.env, "msvc");
        assert!(parse_triple("sparc64-sun-solaris").is_err());
    }

    #[test]
    fn evaluates_predicates() {
        assert!(eval_cfg("unix", "aarch64-apple-darwin").unwrap());
        assert!(!eval_cfg("windows", "aarch64-apple-darwin").unwrap());
        assert!(eval_cfg("cfg(windows)", "x86_64-pc-windows-msvc").unwrap());
        assert!(eval_cfg("target_os = \"macos\"", "aarch64-apple-darwin").unwrap());
        assert!(!eval_cfg("target_os = \"windows\"", "aarch64-apple-darwin").unwrap());
        assert!(eval_cfg("target_arch = \"aarch64\"", "aarch64-apple-darwin").unwrap());
        assert!(eval_cfg("target_env = \"gnu\"", "x86_64-unknown-linux-gnu").unwrap());
        assert!(eval_cfg("target_vendor = \"apple\"", "aarch64-apple-darwin").unwrap());
    }

    #[test]
    fn evaluates_composite_expressions() {
        assert!(eval_cfg("all(unix, target_os = \"macos\")", "aarch64-apple-darwin").unwrap());
        assert!(!eval_cfg("all(unix, target_os = \"linux\")", "aarch64-apple-darwin").unwrap());
        assert!(
            eval_cfg(
                "any(target_os = \"linux\", target_os = \"macos\")",
                "aarch64-apple-darwin"
            )
            .unwrap()
        );
        assert!(eval_cfg("not(windows)", "aarch64-apple-darwin").unwrap());
        assert!(!eval_cfg("not(unix)", "aarch64-apple-darwin").unwrap());
        assert!(
            eval_cfg(
                "all(not(windows), any(target_env = \"\", target_env = \"gnu\"))",
                "aarch64-apple-darwin"
            )
            .unwrap()
        );
    }

    #[test]
    fn rejects_unsupported_and_malformed() {
        assert!(matches!(
            eval_cfg("feature = \"foo\"", "aarch64-apple-darwin"),
            Err(CfgError::Unsupported(_))
        ));
        assert!(eval_cfg("target_pointer_width = \"64\"", "aarch64-apple-darwin").unwrap());
        assert!(!eval_cfg("target_pointer_width = \"32\"", "aarch64-apple-darwin").unwrap());
        // Custom predicates are never set by the compiler: false, not an
        // error (tokio's `cfg(loom)` tables are dropped, cargo-style).
        assert!(!eval_cfg("loom", "aarch64-apple-darwin").unwrap());
        assert!(!eval_cfg("target_unknown = \"x\"", "aarch64-apple-darwin").unwrap());
        // Empty composites: `all()` true, `any()` false (generated
        // manifests emit `cfg(any())`).
        assert!(eval_cfg("all()", "aarch64-apple-darwin").unwrap());
        assert!(!eval_cfg("any()", "aarch64-apple-darwin").unwrap());
        assert!(eval_cfg("all(unix", "aarch64-apple-darwin").is_err());
        assert!(eval_cfg("", "aarch64-apple-darwin").is_err());
    }
}
