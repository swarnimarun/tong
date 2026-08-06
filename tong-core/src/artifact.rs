//! Content-addressed artifact references.
//!
//! Actions never name host paths: executables, inputs, and outputs are
//! referenced by digest (PLAN.md section 4.2). A [`BlobDigest`] identifies a
//! single file's contents; a [`TreeDigest`] identifies the canonical
//! encoding of a [`crate::tree::Tree`]; [`ArtifactRef`] is the union used
//! for executables.

use crate::canonical::{CanonicalEncode, Encoder};
use crate::digest::Digest;
use crate::paths::RelativePath;

/// Digest of a single file's contents.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct BlobDigest(Digest);

impl BlobDigest {
    /// Wraps a digest as a blob digest.
    pub const fn new(digest: Digest) -> Self {
        Self(digest)
    }

    /// Returns the underlying digest.
    pub const fn digest(self) -> Digest {
        self.0
    }
}

impl CanonicalEncode for BlobDigest {
    fn encode(&self, enc: &mut Encoder) {
        self.0.encode(enc);
    }
}

/// Digest of the canonical encoding of a [`crate::tree::Tree`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TreeDigest(Digest);

impl TreeDigest {
    /// Wraps a digest as a tree digest.
    pub const fn new(digest: Digest) -> Self {
        Self(digest)
    }

    /// Returns the underlying digest.
    pub const fn digest(self) -> Digest {
        self.0
    }
}

impl CanonicalEncode for TreeDigest {
    fn encode(&self, enc: &mut Encoder) {
        self.0.encode(enc);
    }
}

/// A reference to content in the store.
///
/// Discriminant order is part of the canonical encoding; do not reorder
/// without a schema-version bump.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ArtifactRef {
    /// A single file.
    Blob(BlobDigest),
    /// A directory tree.
    Tree(TreeDigest),
    /// A single file inside a tree — e.g. an executable produced by another
    /// action, such as a compiled build script.
    TreeFile {
        /// The containing tree.
        tree: TreeDigest,
        /// Path of the file within the tree.
        path: RelativePath,
    },
}

impl CanonicalEncode for ArtifactRef {
    fn encode(&self, enc: &mut Encoder) {
        match self {
            Self::Blob(digest) => {
                enc.write_u32(0);
                digest.encode(enc);
            }
            Self::Tree(digest) => {
                enc.write_u32(1);
                digest.encode(enc);
            }
            Self::TreeFile { tree, path } => {
                enc.write_u32(2);
                tree.encode(enc);
                path.encode(enc);
            }
        }
    }
}
