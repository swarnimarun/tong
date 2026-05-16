use crate::error::{IoContext, Result};
use std::fs;
use std::io::{self, Read};
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
        self.inner.finalize().to_hex().to_string()
    }
}

impl Default for StableHasher {
    fn default() -> Self {
        Self::new()
    }
}

pub fn hash_file(path: &Path) -> Result<String> {
    let file = fs::File::open(path).with_context(format!("failed to read {}", path.display()))?;
    hash_reader(file).with_context(format!("failed to read {}", path.display()))
}

pub fn hash_reader(mut reader: impl Read) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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
    fn hash_bytes_uses_blake3() {
        assert_eq!(
            hash_bytes(b"tong"),
            "e4894a20d67718699851c96c81bd8ae7e76a89e6f3dfc8cdcfd00bfe70c95fd8"
        );
        assert_ne!(hash_bytes(b"tong"), hash_bytes(b"tang"));
    }

    #[test]
    fn hash_reader_matches_hash_file() {
        let path = std::env::temp_dir().join(format!(
            "tong-hash-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, "contents").unwrap();
        let file_hash = hash_file(&path).unwrap();
        let reader_hash = hash_reader(&b"contents"[..]).unwrap();
        assert_eq!(file_hash, reader_hash);
        fs::remove_file(path).unwrap();
    }
}
