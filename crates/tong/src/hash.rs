use crate::error::{IoContext, Result};
use std::fs;
use std::path::Path;

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

#[derive(Debug, Clone)]
pub struct StableHasher {
    state: u64,
}

impl StableHasher {
    pub fn new() -> Self {
        Self { state: FNV_OFFSET }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(FNV_PRIME);
        }
    }

    pub fn update_str(&mut self, value: &str) {
        self.update(value.as_bytes());
        self.update(&[0]);
    }

    pub fn finish_hex(&self) -> String {
        format!("{:016x}", self.state)
    }
}

pub fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(format!("failed to read {}", path.display()))?;
    let mut hasher = StableHasher::new();
    hasher.update(&bytes);
    Ok(hasher.finish_hex())
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = StableHasher::new();
    hasher.update(bytes);
    hasher.finish_hex()
}
