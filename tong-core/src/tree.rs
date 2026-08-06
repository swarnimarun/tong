//! Canonical directory trees.
//!
//! A [`Tree`] is the canonical representation of a directory: action input
//! roots, toolchain closures, environment-bundle files, and outputs are all
//! trees identified by [`TreeDigest`] (PLAN.md sections 4.5 and 10).
//!
//! Canonicalization rules (section 4.5):
//!
//! - Entries are sorted by name (byte-wise); construction order never
//!   affects the digest.
//! - File modes are normalized to a single executable bit; permission bits,
//!   ownership, timestamps, and xattrs never enter the encoding.
//! - Symlink targets are stored as opaque, unnormalized strings.
//! - Entry names are single components: no `/`, `\`, NUL, `.`, or `..`.
//! - Names are significant in case and are never folded.

use std::collections::BTreeMap;
use std::fmt;

use crate::digest::Digest;

use crate::artifact::{BlobDigest, TreeDigest};
use crate::canonical::{CanonicalDecode, CanonicalEncode, DecodeError, Decoder, Encoder};

/// Schema version of the tree encoding.
pub const TREE_SCHEMA_VERSION: u32 = 1;

/// A single tree entry.
///
/// Discriminant order is part of the canonical encoding; do not reorder
/// without bumping [`TREE_SCHEMA_VERSION`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TreeEntry {
    /// A regular file with normalized mode.
    File {
        /// Content digest of the file.
        digest: BlobDigest,
        /// The only surviving mode bit.
        executable: bool,
    },
    /// A symbolic link; the target is opaque and never resolved or
    /// normalized by the encoding.
    Symlink {
        /// The link target, exactly as stored.
        target: String,
    },
    /// A subdirectory, referenced by its own tree digest.
    Directory(TreeDigest),
}

impl CanonicalEncode for TreeEntry {
    fn encode(&self, enc: &mut Encoder) {
        match self {
            Self::File { digest, executable } => {
                enc.write_u32(0);
                digest.encode(enc);
                executable.encode(enc);
            }
            Self::Symlink { target } => {
                enc.write_u32(1);
                target.encode(enc);
            }
            Self::Directory(digest) => {
                enc.write_u32(2);
                digest.encode(enc);
            }
        }
    }
}

impl CanonicalDecode for TreeEntry {
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match dec.read_discriminant()? {
            0 => Ok(Self::File {
                digest: BlobDigest::new(Digest::decode(dec)?),
                executable: bool::decode(dec)?,
            }),
            1 => Ok(Self::Symlink {
                target: String::decode(dec)?,
            }),
            2 => Ok(Self::Directory(TreeDigest::new(Digest::decode(dec)?))),
            tag => Err(DecodeError::InvalidTag(tag)),
        }
    }
}

/// A canonical directory tree.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Tree {
    entries: BTreeMap<String, TreeEntry>,
}

impl Tree {
    /// Creates a tree, validating entry names.
    pub fn new(entries: BTreeMap<String, TreeEntry>) -> Result<Self, TreeError> {
        for name in entries.keys() {
            validate_name(name)?;
        }
        Ok(Self { entries })
    }

    /// Returns the entry map.
    pub fn entries(&self) -> &BTreeMap<String, TreeEntry> {
        &self.entries
    }

    /// Returns the tree digest: `SHA-256(schema_version || entries)`.
    pub fn digest(&self) -> TreeDigest {
        let mut enc = Encoder::new();
        enc.write_u32(TREE_SCHEMA_VERSION);
        self.encode(&mut enc);
        TreeDigest::new(enc.digest())
    }
}

impl CanonicalEncode for Tree {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_map(&self.entries);
    }
}

impl CanonicalDecode for Tree {
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let entries = BTreeMap::<String, TreeEntry>::decode(dec)?;
        Tree::new(entries).map_err(|err| DecodeError::InvalidValue(err.to_string()))
    }
}

fn validate_name(name: &str) -> Result<(), TreeError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(TreeError::InvalidName(name.to_owned()));
    }
    Ok(())
}

/// Error returned when a tree fails validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// An entry name is not a single valid path component.
    InvalidName(String),
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(f, "invalid tree entry name {name:?}"),
        }
    }
}

impl std::error::Error for TreeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::Hasher;

    fn blob(byte: u8) -> BlobDigest {
        BlobDigest::new(Hasher::digest(&[byte]))
    }

    #[test]
    fn rejects_invalid_names() {
        for name in ["", ".", "..", "a/b", "a\\b"] {
            let entries = BTreeMap::from([(name.to_owned(), TreeEntry::Directory(empty_digest()))]);
            assert!(
                matches!(Tree::new(entries), Err(TreeError::InvalidName(_))),
                "{name:?}"
            );
        }
    }

    #[test]
    fn digest_is_insertion_order_independent() {
        let forward = Tree::new(BTreeMap::from([
            ("a".to_owned(), file(blob(1), false)),
            ("b".to_owned(), file(blob(2), true)),
        ]))
        .unwrap();
        let mut reversed_map = BTreeMap::new();
        reversed_map.insert("b".to_owned(), file(blob(2), true));
        reversed_map.insert("a".to_owned(), file(blob(1), false));
        let reversed = Tree::new(reversed_map).unwrap();
        assert_eq!(forward.digest(), reversed.digest());
    }

    #[test]
    fn schema_version_changes_the_digest() {
        let tree = Tree::new(BTreeMap::new()).unwrap();
        let mut enc = Encoder::new();
        enc.write_u32(TREE_SCHEMA_VERSION + 1);
        tree.encode(&mut enc);
        assert_ne!(tree.digest(), TreeDigest::new(enc.digest()));
    }

    fn file(digest: BlobDigest, executable: bool) -> TreeEntry {
        TreeEntry::File { digest, executable }
    }

    fn empty_digest() -> TreeDigest {
        Tree::default().digest()
    }
}
