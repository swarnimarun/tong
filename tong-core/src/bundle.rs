//! Environment bundles.
//!
//! A bundle represents both environment variables and the artifacts those
//! variables reference — for example an MSVC bundle carrying `INCLUDE` and
//! `LIB` together with the compiler binaries, SDK headers, and import
//! libraries (PLAN.md section 5). The bundle digest covers the referenced
//! file closure, not merely the variable text.
//!
//! Merge order at execution time is: deterministic base environment, then
//! bundle variables, then per-action environment (the per-action environment
//! wins conflicts). The parent process environment is never inherited.

use std::collections::BTreeMap;

use crate::action::CanonicalValue;
use crate::artifact::TreeDigest;
use crate::canonical::{CanonicalDecode, CanonicalEncode, Decoder, Encoder};
use crate::digest::Digest;
use crate::platform::PlatformKey;

/// Schema version of the environment-bundle encoding.
pub const ENVIRONMENT_BUNDLE_SCHEMA_VERSION: u32 = 1;

/// An environment bundle: variables plus the fingerprinted file closure they
/// reference (PLAN.md section 5).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EnvironmentBundle {
    /// Human-readable bundle name, e.g. `msvc-14.44-windows-x86_64`.
    pub name: String,
    /// Bundle provider identity, e.g. `nix`, `download`, `system-capture`.
    pub provider: String,
    /// Platform the bundle runs on.
    pub platform: PlatformKey,

    /// Variables contributed by the bundle.
    pub variables: BTreeMap<String, String>,
    /// Every file the variables (and the tools) reference.
    pub files: TreeDigest,
    /// Provider-specific metadata, canonically encoded.
    pub metadata: BTreeMap<String, CanonicalValue>,
}

impl EnvironmentBundle {
    /// Returns the bundle digest: `SHA-256(schema_version || fields)`.
    ///
    /// Because `files` is the tree digest of the whole referenced closure,
    /// changing any referenced file changes the bundle digest (PLAN.md
    /// section 5).
    pub fn digest(&self) -> Digest {
        let mut enc = Encoder::new();
        enc.write_u32(ENVIRONMENT_BUNDLE_SCHEMA_VERSION);
        self.encode(&mut enc);
        enc.digest()
    }
}

impl CanonicalEncode for EnvironmentBundle {
    fn encode(&self, enc: &mut Encoder) {
        self.name.encode(enc);
        self.provider.encode(enc);
        self.platform.encode(enc);
        self.variables.encode(enc);
        self.files.encode(enc);
        self.metadata.encode(enc);
    }
}

impl CanonicalDecode for EnvironmentBundle {
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, crate::canonical::DecodeError> {
        Ok(Self {
            name: String::decode(dec)?,
            provider: String::decode(dec)?,
            platform: PlatformKey::new(BTreeMap::decode(dec)?),
            variables: BTreeMap::decode(dec)?,
            files: TreeDigest::new(Digest::decode(dec)?),
            metadata: BTreeMap::<String, CanonicalValue>::decode(dec)?,
        })
    }
}

/// A reference to an [`EnvironmentBundle`] by digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct BundleRef(Digest);

impl BundleRef {
    /// References a bundle by its digest.
    pub fn of(bundle: &EnvironmentBundle) -> Self {
        Self(bundle.digest())
    }

    /// Wraps a digest as a bundle reference.
    pub const fn new(digest: Digest) -> Self {
        Self(digest)
    }

    /// Returns the underlying digest.
    pub const fn digest(self) -> Digest {
        self.0
    }
}

impl CanonicalEncode for BundleRef {
    fn encode(&self, enc: &mut Encoder) {
        self.0.encode(enc);
    }
}
