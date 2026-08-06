//! The versioned action schema.
//!
//! An [`ActionSpec`] is the only execution unit in Tong (PLAN.md section
//! 3.1). Its *semantic digest* — the cache identity — covers exactly the
//! fields that influence execution (section 4.4):
//!
//! ```text
//! SHA-256(schema_version || semantic fields || input_root || environment_bundle)
//! ```
//!
//! `logical_id` and `mnemonic` are graph identity and diagnostic metadata:
//! they are never part of the semantic digest, so renaming a target or
//! relocating a workspace never invalidates caches (section 4.1).

use std::collections::BTreeMap;
use std::time::Duration;

use crate::artifact::{ArtifactRef, TreeDigest};
use crate::bundle::BundleRef;
use crate::canonical::{CanonicalEncode, Encoder};
use crate::digest::Digest;
use crate::paths::{OutputPath, RelativePath};
use crate::platform::PlatformKey;

/// Schema version of the action encoding.
pub const ACTION_SCHEMA_VERSION: u32 = 1;

/// Placeholder substituted with the action's exec root path at execution
/// time.
pub const EXEC_ROOT_VAR: &str = "{exec_root}";

/// Placeholder substituted with the environment bundle's local root at
/// execution time.
pub const BUNDLE_ROOT_VAR: &str = "{bundle_root}";

/// Graph identity of an action, used for diagnostics and queries.
///
/// Never encoded into the semantic digest (PLAN.md section 4.1).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ActionId(pub String);

impl CanonicalEncode for ActionId {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_str(&self.0);
    }
}

/// A command-line argument.
///
/// Schema version 1 carries plain strings only; sandbox path remapping makes
/// input and output locations deterministic. Typed artifact/output
/// references are a planned extension and require bumping
/// [`ACTION_SCHEMA_VERSION`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Argument(pub String);

impl CanonicalEncode for Argument {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_str(&self.0);
    }
}

/// Whether an action may access the network.
///
/// Discriminant order is part of the canonical encoding; do not reorder
/// without bumping [`ACTION_SCHEMA_VERSION`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetworkPolicy {
    /// No network access. The default for every action; enforced from
    /// sandbox level 4 (PLAN.md section 11).
    Deny,
    /// Network access permitted. Only fixed-output fetch actions — which
    /// know their expected digest before executing — may use this
    /// (PLAN.md section 9).
    Allow,
}

impl CanonicalEncode for NetworkPolicy {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_u32(match self {
            Self::Deny => 0,
            Self::Allow => 1,
        });
    }
}

/// Whether an action's result may be cached and shared.
///
/// Discriminant order is part of the canonical encoding; do not reorder
/// without bumping [`ACTION_SCHEMA_VERSION`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CachePolicy {
    /// Result may be memoized, cached locally, and published to shared
    /// caches subject to hermeticity level.
    Enabled,
    /// Result must never be cached or uploaded — for example actions
    /// receiving secrets (PLAN.md section 5).
    Disabled,
}

impl CanonicalEncode for CachePolicy {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_u32(match self {
            Self::Enabled => 0,
            Self::Disabled => 1,
        });
    }
}

/// Declared resource needs of an action, used by the scheduler and remote
/// executor matching.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ResourceRequirements {
    /// CPU cores requested.
    pub cpu_cores: Option<u32>,
    /// Memory requested, in bytes.
    pub memory_bytes: Option<u64>,
    /// Scratch disk requested, in bytes.
    pub disk_bytes: Option<u64>,
}

impl CanonicalEncode for ResourceRequirements {
    fn encode(&self, enc: &mut Encoder) {
        self.cpu_cores.encode(enc);
        self.memory_bytes.encode(enc);
        self.disk_bytes.encode(enc);
    }
}

/// A typed property value in `ActionSpec::properties`.
///
/// Replaces the unconstrained "extra key material" of the old model
/// (PLAN.md section 4.3). All values use this canonical encoding.
///
/// Discriminant order is part of the canonical encoding; do not reorder
/// without bumping the schema version of the containing structure.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CanonicalValue {
    /// A boolean.
    Bool(bool),
    /// A signed 64-bit integer.
    Int(i64),
    /// A UTF-8 string.
    String(String),
}

impl CanonicalEncode for CanonicalValue {
    fn encode(&self, enc: &mut Encoder) {
        match self {
            Self::Bool(value) => {
                enc.write_u32(0);
                value.encode(enc);
            }
            Self::Int(value) => {
                enc.write_u32(1);
                value.encode(enc);
            }
            Self::String(value) => {
                enc.write_u32(2);
                value.encode(enc);
            }
        }
    }
}

impl crate::canonical::CanonicalDecode for CanonicalValue {
    fn decode(
        dec: &mut crate::canonical::Decoder<'_>,
    ) -> Result<Self, crate::canonical::DecodeError> {
        use crate::canonical::DecodeError;
        match dec.read_discriminant()? {
            0 => Ok(Self::Bool(bool::decode(dec)?)),
            1 => Ok(Self::Int(i64::decode(dec)?)),
            2 => Ok(Self::String(String::decode(dec)?)),
            tag => Err(DecodeError::InvalidTag(tag)),
        }
    }
}

/// The versioned action schema (PLAN.md section 4.4).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ActionSpec {
    /// Version of this schema; always [`ACTION_SCHEMA_VERSION`] for new
    /// actions. Part of the digest, so schema changes invalidate caches.
    pub schema_version: u32,

    /// Graph identity for diagnostics and queries. Not digested.
    pub logical_id: ActionId,
    /// Diagnostic name of the action kind, e.g. `RustCompile`. Not digested
    /// unless it changes execution semantics (then it belongs in
    /// `properties`).
    pub mnemonic: String,

    /// The executable, as store content — never a host path (section 4.2).
    pub executable: ArtifactRef,
    /// Command-line arguments.
    pub arguments: Vec<Argument>,
    /// Environment bundle supplying toolchains, SDKs, and their variables
    /// (section 5).
    pub environment_bundle: Option<BundleRef>,
    /// Per-action environment; wins over bundle and base on conflicts.
    pub environment: BTreeMap<String, String>,

    /// Merkle root of every input: sources, generated artifacts, compiler
    /// binaries, SDK content, dependency outputs.
    pub input_root: TreeDigest,
    /// Outputs the action must produce; results failing validation are
    /// rejected (section 4.6).
    pub declared_outputs: Vec<OutputPath>,
    /// Working directory inside the input root.
    pub working_directory: RelativePath,

    /// Where the action executes.
    pub execution_platform: PlatformKey,
    /// Where the resulting artifact runs, when relevant.
    pub target_platform: Option<PlatformKey>,

    /// Execution time limit.
    pub timeout: Option<Duration>,
    /// Network access policy.
    pub network_policy: NetworkPolicy,
    /// Cache eligibility.
    pub cache_policy: CachePolicy,
    /// Declared resource needs.
    pub resource_requirements: ResourceRequirements,

    /// Namespaced, canonically encoded extra key material (section 4.3),
    /// e.g. `tong.rust.profile` or `tong.execution.network_policy`.
    pub properties: BTreeMap<String, CanonicalValue>,
}

impl ActionSpec {
    /// Returns the semantic execution digest used for caching.
    ///
    /// Covers the schema version and every field that influences execution —
    /// including the input root and environment bundle — but never
    /// `logical_id` or `mnemonic` (PLAN.md sections 4.1 and 4.4).
    pub fn digest(&self) -> Digest {
        let mut enc = Encoder::new();
        enc.write_u32(self.schema_version);
        self.encode_semantic(&mut enc);
        enc.digest()
    }

    /// Encodes only the fields that influence execution.
    fn encode_semantic(&self, enc: &mut Encoder) {
        self.executable.encode(enc);
        self.arguments.encode(enc);
        self.environment_bundle.encode(enc);
        self.environment.encode(enc);
        self.input_root.encode(enc);
        self.declared_outputs.encode(enc);
        self.working_directory.encode(enc);
        self.execution_platform.encode(enc);
        self.target_platform.encode(enc);
        self.timeout.encode(enc);
        self.network_policy.encode(enc);
        self.cache_policy.encode(enc);
        self.resource_requirements.encode(enc);
        self.properties.encode(enc);
    }
}

impl CanonicalEncode for ActionSpec {
    /// Full encoding for storage and debugging: identity fields first, then
    /// the semantic fields. Cache keys must use [`ActionSpec::digest`], not
    /// this encoding.
    fn encode(&self, enc: &mut Encoder) {
        self.logical_id.encode(enc);
        self.mnemonic.encode(enc);
        self.encode_semantic(enc);
    }
}
