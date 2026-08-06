//! Target labels.
//!
//! Labels name targets: `:name` within the workspace, or `//path:name` /
//! `//path` for nested locations. Version 1 of the model only requires the
//! workspace-local form; the package part is preserved for diagnostics.

use std::fmt;

/// A target label.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Label {
    /// Package part (empty for workspace-local labels).
    pub package: String,
    /// Target name.
    pub name: String,
}

impl Label {
    /// Parses a label: `:name`, `//path:name`, or `//path`.
    pub fn parse(text: &str) -> Result<Self, LabelError> {
        if let Some(name) = text.strip_prefix(':') {
            if name.is_empty() {
                return Err(LabelError::Empty(text.to_owned()));
            }
            return Ok(Self {
                package: String::new(),
                name: name.to_owned(),
            });
        }
        if let Some(rest) = text.strip_prefix("//") {
            let (package, name) = match rest.split_once(':') {
                Some((package, name)) => (package, name),
                None => {
                    let package = rest;
                    let name = package.rsplit('/').next().unwrap_or(package);
                    (package, name)
                }
            };
            if package.is_empty() || name.is_empty() {
                return Err(LabelError::Empty(text.to_owned()));
            }
            return Ok(Self {
                package: package.to_owned(),
                name: name.to_owned(),
            });
        }
        Err(LabelError::Malformed(text.to_owned()))
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.package.is_empty() {
            write!(f, ":{}", self.name)
        } else {
            write!(f, "//{}:{}", self.package, self.name)
        }
    }
}

/// Label parsing failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LabelError {
    /// The label was empty.
    Empty(String),
    /// The label was not in a recognized form.
    Malformed(String),
}

impl fmt::Display for LabelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(label) => write!(f, "empty label {label:?}"),
            Self::Malformed(label) => write!(f, "malformed label {label:?}"),
        }
    }
}

impl std::error::Error for LabelError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_labels() {
        let label = Label::parse(":hello").unwrap();
        assert_eq!(label.package, "");
        assert_eq!(label.name, "hello");
    }

    #[test]
    fn parses_absolute_labels() {
        let label = Label::parse("//crates/codec:codec").unwrap();
        assert_eq!(label.package, "crates/codec");
        assert_eq!(label.name, "codec");
        let label = Label::parse("//crates/codec").unwrap();
        assert_eq!(label.name, "codec");
    }

    #[test]
    fn rejects_malformed_labels() {
        assert!(Label::parse("").is_err());
        assert!(Label::parse(":").is_err());
        assert!(Label::parse("//:x").is_err());
        assert!(Label::parse("hello").is_err());
    }
}
