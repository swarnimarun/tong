//! The canonical binary encoding.
//!
//! Every hashed structure in Tong — actions, trees, environment bundles,
//! manifests — is encoded through this module before hashing (PLAN.md
//! section 4.5). The encoding is the cross-platform contract: Linux, macOS,
//! and Windows must produce byte-identical encodings for the same logical
//! value, or shared caching is impossible.
//!
//! ## Rules
//!
//! - Integers are fixed-width little-endian two's complement.
//! - Booleans are a single byte: `0x00` or `0x01`.
//! - Byte strings and UTF-8 strings are a `u64` length followed by the raw
//!   bytes. Strings are never normalized, case-folded, or NUL-terminated.
//! - `Option` is a tag byte (`0x00` = `None`, `0x01` = `Some`) followed by
//!   the value when present.
//! - Sequences are a `u64` count followed by the elements in order.
//! - Maps are [`BTreeMap`]s, encoded as a `u64` count followed by key/value
//!   pairs in ascending key order (byte-wise for strings).
//! - Enums are a `u32` discriminant followed by the variant payload.
//!   Discriminants follow declaration order starting at 0; reordering
//!   variants is a breaking change requiring a schema-version bump.
//! - Structs are their fields in declaration order, with no field names;
//!   schema evolution happens through explicit schema versions.
//!
//! Unknown fields are never permitted: an encoding is always complete, so
//! two implementations either agree on a schema version or reject each
//! other's data.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::digest::{Digest, Hasher};

/// Encodes a value into its canonical binary form.
///
/// Implementations must follow the module-level rules exactly; this trait is
/// the cache-correctness boundary, so it is implemented by hand for every
/// schema type rather than derived.
pub trait CanonicalEncode {
    /// Appends the canonical encoding of `self` to `enc`.
    fn encode(&self, enc: &mut Encoder);
}

/// An append-only canonical encoder.
///
/// Bytes are buffered so the same encoder can produce either the raw
/// encoding (for storage and golden tests) or its digest (for hashing).
#[derive(Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    /// Creates an empty encoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends raw bytes without a length prefix.
    pub fn write_raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    /// Appends a `u8`.
    pub fn write_u8(&mut self, value: u8) -> &mut Self {
        self.buf.push(value);
        self
    }

    /// Appends a `u32` in little-endian order.
    pub fn write_u32(&mut self, value: u32) -> &mut Self {
        self.write_raw(&value.to_le_bytes())
    }

    /// Appends a `u64` in little-endian order.
    pub fn write_u64(&mut self, value: u64) -> &mut Self {
        self.write_raw(&value.to_le_bytes())
    }

    /// Appends an `i64` in little-endian two's complement.
    pub fn write_i64(&mut self, value: i64) -> &mut Self {
        self.write_raw(&value.to_le_bytes())
    }

    /// Appends a boolean as `0x00` or `0x01`.
    pub fn write_bool(&mut self, value: bool) -> &mut Self {
        self.write_u8(u8::from(value))
    }

    /// Appends a `u64` length followed by the raw bytes.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.write_u64(bytes.len() as u64);
        self.write_raw(bytes)
    }

    /// Appends a `u64` length followed by the UTF-8 bytes.
    pub fn write_str(&mut self, value: &str) -> &mut Self {
        self.write_bytes(value.as_bytes())
    }

    /// Appends an optional value with its tag byte.
    pub fn write_option<T>(&mut self, value: &Option<T>) -> &mut Self
    where
        T: CanonicalEncode,
    {
        match value {
            None => self.write_u8(0),
            Some(inner) => {
                self.write_u8(1);
                inner.encode(self);
                self
            }
        }
    }

    /// Appends a sequence as a `u64` count followed by the elements.
    pub fn write_seq<T>(&mut self, items: &[T]) -> &mut Self
    where
        T: CanonicalEncode,
    {
        self.write_u64(items.len() as u64);
        for item in items {
            item.encode(self);
        }
        self
    }

    /// Appends a map as a `u64` count followed by pairs in ascending key
    /// order.
    pub fn write_map<K, V>(&mut self, map: &BTreeMap<K, V>) -> &mut Self
    where
        K: CanonicalEncode,
        V: CanonicalEncode,
    {
        self.write_u64(map.len() as u64);
        for (key, value) in map {
            key.encode(self);
            value.encode(self);
        }
        self
    }

    /// Returns the digest of everything written so far.
    pub fn digest(&self) -> Digest {
        let mut hasher = Hasher::new();
        hasher.update(&self.buf);
        hasher.finish()
    }

    /// Consumes the encoder and returns the encoded bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Returns the canonical encoding of `value`.
pub fn encode_vec<T>(value: &T) -> Vec<u8>
where
    T: CanonicalEncode + ?Sized,
{
    let mut enc = Encoder::new();
    value.encode(&mut enc);
    enc.into_bytes()
}

/// Returns the digest of the canonical encoding of `value`.
pub fn digest_of<T>(value: &T) -> Digest
where
    T: CanonicalEncode + ?Sized,
{
    let mut enc = Encoder::new();
    value.encode(&mut enc);
    enc.digest()
}

impl CanonicalEncode for u8 {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_u8(*self);
    }
}

impl CanonicalEncode for u32 {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_u32(*self);
    }
}

impl CanonicalEncode for u64 {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_u64(*self);
    }
}

impl CanonicalEncode for i64 {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_i64(*self);
    }
}

impl CanonicalEncode for bool {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_bool(*self);
    }
}

impl CanonicalEncode for str {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_str(self);
    }
}

impl CanonicalEncode for String {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_str(self);
    }
}

impl<T> CanonicalEncode for &T
where
    T: CanonicalEncode + ?Sized,
{
    fn encode(&self, enc: &mut Encoder) {
        (*self).encode(enc);
    }
}

impl<T> CanonicalEncode for Option<T>
where
    T: CanonicalEncode,
{
    fn encode(&self, enc: &mut Encoder) {
        enc.write_option(self);
    }
}

impl<T> CanonicalEncode for Vec<T>
where
    T: CanonicalEncode,
{
    fn encode(&self, enc: &mut Encoder) {
        enc.write_seq(self);
    }
}

impl<K, V> CanonicalEncode for BTreeMap<K, V>
where
    K: CanonicalEncode + Ord,
    V: CanonicalEncode,
{
    fn encode(&self, enc: &mut Encoder) {
        enc.write_map(self);
    }
}

impl CanonicalEncode for Duration {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_u64(self.as_secs());
        enc.write_u32(self.subsec_nanos());
    }
}

impl CanonicalEncode for Digest {
    fn encode(&self, enc: &mut Encoder) {
        // Fixed-width: no length prefix.
        enc.write_raw(self.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes<T: CanonicalEncode + ?Sized>(value: &T) -> Vec<u8> {
        encode_vec(value)
    }

    #[test]
    fn integers_are_little_endian_fixed_width() {
        assert_eq!(bytes(&0x01020304u32), vec![0x04, 0x03, 0x02, 0x01]);
        assert_eq!(bytes(&1u64), vec![1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(bytes(&-1i64), vec![0xff; 8]);
    }

    #[test]
    fn bools_are_single_bytes() {
        assert_eq!(bytes(&false), vec![0x00]);
        assert_eq!(bytes(&true), vec![0x01]);
    }

    #[test]
    fn strings_are_length_prefixed() {
        assert_eq!(bytes("ab"), vec![2, 0, 0, 0, 0, 0, 0, 0, b'a', b'b']);
        assert_eq!(bytes(""), vec![0; 8]);
    }

    #[test]
    fn options_carry_a_tag() {
        assert_eq!(bytes(&None::<u8>), vec![0x00]);
        assert_eq!(bytes(&Some(7u8)), vec![0x01, 0x07]);
    }

    #[test]
    fn sequences_are_count_prefixed() {
        assert_eq!(bytes(&vec![1u8, 2u8]), vec![2, 0, 0, 0, 0, 0, 0, 0, 1, 2]);
    }

    #[test]
    fn maps_encode_in_sorted_order() {
        let mut map = BTreeMap::new();
        map.insert("b".to_string(), 2u8);
        map.insert("a".to_string(), 1u8);
        assert_eq!(
            bytes(&map),
            vec![
                2, 0, 0, 0, 0, 0, 0, 0, // count
                1, 0, 0, 0, 0, 0, 0, 0, b'a', 1, // "a": 1
                1, 0, 0, 0, 0, 0, 0, 0, b'b', 2, // "b": 2
            ]
        );
    }

    #[test]
    fn durations_encode_secs_then_nanos() {
        let duration = Duration::new(5, 7);
        assert_eq!(bytes(&duration), vec![5, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0]);
    }
}
