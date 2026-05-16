use crate::error::{IoContext, Result};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct StableHasher {
    state: blake3::Hasher,
}

impl StableHasher {
    pub fn new() -> Self {
        Self {
            state: blake3::Hasher::new(),
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.state.update(bytes);
    }

    pub fn update_str(&mut self, value: &str) {
        self.update(value.as_bytes());
        self.update(&[0]);
    }

    pub fn finish_hex(&self) -> String {
        self.state.finalize().to_hex().to_string()
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

pub fn hash_reader<R: std::io::Read>(mut reader: R) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut reader, &mut hasher).with_context("failed to read input for hashing")?;
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
    fn hash_reader_matches_hash_bytes() {
        let data = b"hello world";
        let reader = std::io::Cursor::new(data);
        assert_eq!(hash_reader(reader).unwrap(), hash_bytes(data));
    }

    #[test]
    fn hash_file_produces_hex() {
        let path = std::env::temp_dir().join("tong-hash-test.txt");
        fs::write(&path, "test content").unwrap();
        let hash = hash_file(&path).unwrap();
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        fs::remove_file(&path).unwrap();
    }
}
