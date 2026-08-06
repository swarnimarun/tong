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
}
