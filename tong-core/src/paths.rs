//! Normalized relative paths.
//!
//! Paths inside actions, trees, and manifests are always relative,
//! `/`-separated, and free of `.` / `..` / empty components (PLAN.md section
//! 4.5, path normalization rules). Validation happens at construction, so a
//! [`RelativePath`] is canonical by construction.
//!
//! Case is significant and preserved exactly; whether two paths that differ
//! only in case collide is a property of the execution platform's
//! filesystem, not of this encoding.

use std::fmt;

use crate::canonical::{CanonicalEncode, Encoder};

/// A normalized relative path: `/`-separated, no `.`, `..`, or empty
/// components.
///
/// The single component `.` is allowed and denotes the tree root (used for
/// `working_directory`).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RelativePath(String);

impl RelativePath {
    /// The tree-root path, `.`.
    pub const ROOT: &'static str = ".";

    /// Validates and normalizes a path.
    pub fn new(path: &str) -> Result<Self, PathError> {
        if path.is_empty() {
            return Err(PathError::Empty);
        }
        if path == Self::ROOT {
            return Ok(Self(path.to_owned()));
        }
        if path.starts_with('/') || is_windows_absolute(path) {
            return Err(PathError::Absolute);
        }
        if path.contains('\\') {
            return Err(PathError::BackslashSeparator);
        }
        for component in path.split('/') {
            match component {
                "" => return Err(PathError::EmptyComponent),
                "." => return Err(PathError::DotComponent),
                ".." => return Err(PathError::ParentTraversal),
                _ => {}
            }
        }
        Ok(Self(path.to_owned()))
    }

    /// Returns the path as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl CanonicalEncode for RelativePath {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_str(&self.0);
    }
}

/// A declared action output path. Identical rules to [`RelativePath`], but a
/// distinct type so outputs and inputs are never mixed up.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct OutputPath(RelativePath);

impl OutputPath {
    /// Validates and normalizes an output path.
    pub fn new(path: &str) -> Result<Self, PathError> {
        Ok(Self(RelativePath::new(path)?))
    }

    /// Returns the path as a string.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for OutputPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl CanonicalEncode for OutputPath {
    fn encode(&self, enc: &mut Encoder) {
        self.0.encode(enc);
    }
}

/// Detects `C:`-style drive-letter paths.
fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Error returned when a path fails normalization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// The path is empty.
    Empty,
    /// The path is absolute (`/...` or `C:...`); all Tong paths are relative.
    Absolute,
    /// The path contains `\`; use `/` separators on every platform.
    BackslashSeparator,
    /// The path contains an empty component (`a//b` or a trailing `/`).
    EmptyComponent,
    /// The path contains a `.` component; only the whole path `.` (tree root)
    /// is allowed.
    DotComponent,
    /// The path contains a `..` component; paths must not escape their tree.
    ParentTraversal,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "path is empty"),
            Self::Absolute => write!(f, "path is absolute; Tong paths are relative"),
            Self::BackslashSeparator => {
                write!(
                    f,
                    "path contains '\\'; use '/' separators on every platform"
                )
            }
            Self::EmptyComponent => write!(f, "path contains an empty component"),
            Self::DotComponent => write!(f, "path contains a '.' component"),
            Self::ParentTraversal => write!(f, "path contains a '..' component"),
        }
    }
}

impl std::error::Error for PathError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_paths() {
        assert_eq!(RelativePath::new(".").unwrap().as_str(), ".");
        assert_eq!(RelativePath::new("a").unwrap().as_str(), "a");
        assert_eq!(
            RelativePath::new("crates/codec/src/lib.rs")
                .unwrap()
                .as_str(),
            "crates/codec/src/lib.rs"
        );
    }

    #[test]
    fn rejects_non_canonical_paths() {
        assert_eq!(RelativePath::new(""), Err(PathError::Empty));
        assert_eq!(RelativePath::new("/abs/path"), Err(PathError::Absolute));
        assert_eq!(RelativePath::new("C:/sdk/lib"), Err(PathError::Absolute));
        assert_eq!(
            RelativePath::new("a\\b"),
            Err(PathError::BackslashSeparator)
        );
        assert_eq!(RelativePath::new("a//b"), Err(PathError::EmptyComponent));
        assert_eq!(RelativePath::new("a/"), Err(PathError::EmptyComponent));
        assert_eq!(RelativePath::new("a/./b"), Err(PathError::DotComponent));
        assert_eq!(RelativePath::new("../x"), Err(PathError::ParentTraversal));
    }

    #[test]
    fn output_paths_follow_the_same_rules() {
        assert!(OutputPath::new("lib/libcodec.a").is_ok());
        assert_eq!(OutputPath::new("/tmp/x"), Err(PathError::Absolute));
    }
}
