use crate::error::{IoContext, Result};
use std::fs;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct StableHasher {
    hasher: blake3::Hasher,
}

impl StableHasher {
    pub fn new() -> Self {
        Self {
            hasher: blake3::Hasher::new(),
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    pub fn update_str(&mut self, value: &str) {
        self.update(value.as_bytes());
        self.update(&[0]);
    }

    pub fn finish_hex(&self) -> String {
        self.hasher.finalize().to_hex().to_string()
    }
}

impl Default for StableHasher {
    fn default() -> Self {
        Self::new()
    }
}

pub fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(format!("failed to read {}", path.display()))?;
    Ok(hash_bytes(&bytes))
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn hash_reader(reader: &mut impl Read) -> std::io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
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
    fn hash_matches_fixed_length() {
        let result = hash_bytes(b"hello");
        assert_eq!(result.len(), 64);
    }
}
