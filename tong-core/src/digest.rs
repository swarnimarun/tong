//! Content digests and the canonical hasher.
//!
//! SHA-256 is the single mandatory digest algorithm for every content-
//! addressed object in Tong (PLAN.md section 4.5). A [`Digest`] is an opaque
//! 32-byte value; the lowercase hex form is the canonical text
//! representation used in diagnostics, lockfiles, and store paths.
//!
//! Hex encoding and decoding are hand-rolled to keep the dependency surface
//! of this crate at exactly `sha2`.

use std::fmt;

use sha2::{Digest as _, Sha256};

/// Length of a digest in bytes.
pub const DIGEST_LEN: usize = 32;

/// Length of a digest in lowercase hex characters.
pub const DIGEST_HEX_LEN: usize = DIGEST_LEN * 2;

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// A SHA-256 content digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest([u8; DIGEST_LEN]);

impl Digest {
    /// Wraps raw digest bytes.
    pub const fn from_bytes(bytes: [u8; DIGEST_LEN]) -> Self {
        Self(bytes)
    }

    /// Returns the raw digest bytes.
    pub const fn as_bytes(&self) -> &[u8; DIGEST_LEN] {
        &self.0
    }

    /// Returns the canonical lowercase hex representation.
    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(DIGEST_HEX_LEN);
        for byte in self.0 {
            out.push(HEX_DIGITS[(byte >> 4) as usize] as char);
            out.push(HEX_DIGITS[(byte & 0x0f) as usize] as char);
        }
        out
    }

    /// Parses a hex digest, accepting upper- or lowercase input.
    pub fn from_hex(hex: &str) -> Result<Self, ParseDigestError> {
        if hex.len() != DIGEST_HEX_LEN {
            return Err(ParseDigestError::InvalidLength(hex.len()));
        }
        let mut bytes = [0u8; DIGEST_LEN];
        for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_value(pair[0])?;
            let low = hex_value(pair[1])?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({self})")
    }
}

fn hex_value(c: u8) -> Result<u8, ParseDigestError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(ParseDigestError::InvalidCharacter(c as char)),
    }
}

/// Error returned when parsing a hex digest fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseDigestError {
    /// The input was not exactly [`DIGEST_HEX_LEN`] characters.
    InvalidLength(usize),
    /// The input contained a non-hex character.
    InvalidCharacter(char),
}

impl fmt::Display for ParseDigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength(len) => {
                write!(
                    f,
                    "digest hex must be {DIGEST_HEX_LEN} characters, got {len}"
                )
            }
            Self::InvalidCharacter(c) => write!(f, "invalid hex character {c:?}"),
        }
    }
}

impl std::error::Error for ParseDigestError {}

/// An incremental SHA-256 hasher.
///
/// Callers should normally prefer [`crate::canonical::Encoder`], which feeds
/// canonically encoded bytes into a `Hasher`; use this directly only for raw
/// content such as file blobs.
#[derive(Clone)]
pub struct Hasher(Sha256);

impl Hasher {
    /// Creates a new hasher.
    pub fn new() -> Self {
        Self(Sha256::new())
    }

    /// Feeds bytes into the hasher.
    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }

    /// Returns the digest of `bytes` in one call.
    pub fn digest(bytes: &[u8]) -> Digest {
        let mut hasher = Self::new();
        hasher.update(bytes);
        hasher.finish()
    }

    /// Consumes the hasher and returns the digest.
    pub fn finish(self) -> Digest {
        Digest(self.0.finalize().into())
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        // NIST-known SHA-256 test vectors anchor the hasher itself,
        // independent of any Tong schema.
        assert_eq!(
            Hasher::digest(b"").to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            Hasher::digest(b"abc").to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hex_roundtrip() {
        let digest = Hasher::digest(b"tong");
        let parsed = Digest::from_hex(&digest.to_hex()).unwrap();
        assert_eq!(digest, parsed);
    }

    #[test]
    fn from_hex_accepts_uppercase() {
        let lower = Hasher::digest(b"tong").to_hex();
        let upper = lower.to_uppercase();
        assert_eq!(Digest::from_hex(&lower), Digest::from_hex(&upper));
    }

    #[test]
    fn from_hex_rejects_bad_input() {
        assert_eq!(
            Digest::from_hex("abcd"),
            Err(ParseDigestError::InvalidLength(4))
        );
        let bad = format!("{}zz", "ab".repeat(31));
        assert_eq!(
            Digest::from_hex(&bad),
            Err(ParseDigestError::InvalidCharacter('z'))
        );
    }
}
