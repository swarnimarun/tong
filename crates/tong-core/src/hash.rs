use crate::error::{IoContext, Result};
use std::fs;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct StableHasher {
    inner: blake3::Hasher,
}

impl StableHasher {
    pub fn new() -> Self {
        Self {
            inner: blake3::Hasher::new(),
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    pub fn update_str(&mut self, value: &str) {
        self.update(value.as_bytes());
        self.update(&[0]);
    }

    pub fn finish_hex(&self) -> String {
        let hasher = self.inner.clone();
        hasher.finalize().to_hex().to_string()
    }
}

impl Default for StableHasher {
    fn default() -> Self {
        Self::new()
    }
}

pub fn hash_reader<R: Read>(reader: &mut R) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context("failed to read data for hashing")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(format!("failed to read {}", path.display()))?;
    Ok(hash_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_hasher_separates_string_boundaries() {
        let mut one = StableHasher::new();
        one.update_str("ab");
        one.update_str("c");

        let mut two = StableHasher::new();
        two.update_str("a");
        two.update_str("bc");

        assert_ne!(one.finish_hex(), two.finish_hex());
    }

    #[test]
    fn hash_bytes_is_stable() {
        assert_eq!(hash_bytes(b"tong"), hash_bytes(b"tong"));
        assert_ne!(hash_bytes(b"tong"), hash_bytes(b"tang"));
    }

    #[test]
    fn hash_bytes_produces_64_char_hex() {
        let hex = hash_bytes(b"test");
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_reader_matches_hash_bytes() {
        let data = b"hello world";
        let mut cursor = std::io::Cursor::new(data);
        let reader_hash = hash_reader(&mut cursor).unwrap();
        let bytes_hash = hash_bytes(data);
        assert_eq!(reader_hash, bytes_hash);
    }
}
