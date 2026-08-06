//! The local action cache.
//!
//! Maps an action's semantic digest to its recorded result. Only successful,
//! fully validated results are ever committed (PLAN.md sections 4.6 and
//! 10.3); failed and partial results are discarded. Cached results are
//! immutable and keyed purely by digest, so no whole-build locking is
//! needed.

use std::fs;
use std::io;
use std::path::PathBuf;

use tong_core::artifact::{BlobDigest, TreeDigest};
use tong_core::canonical::{self, CanonicalDecode, CanonicalEncode, DecodeError, Decoder, Encoder};
use tong_core::digest::Digest;

use crate::cas::Cas;

/// A recorded successful action result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CachedResult {
    /// Tree of the action's captured output directory.
    pub outputs: TreeDigest,
    /// Captured stdout.
    pub stdout: BlobDigest,
    /// Captured stderr.
    pub stderr: BlobDigest,
    /// Wall-clock execution time, informational only.
    pub duration_millis: u64,
}

impl CanonicalEncode for CachedResult {
    fn encode(&self, enc: &mut Encoder) {
        self.outputs.encode(enc);
        self.stdout.encode(enc);
        self.stderr.encode(enc);
        self.duration_millis.encode(enc);
    }
}

impl CanonicalDecode for CachedResult {
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            outputs: TreeDigest::new(Digest::decode(dec)?),
            stdout: BlobDigest::new(Digest::decode(dec)?),
            stderr: BlobDigest::new(Digest::decode(dec)?),
            duration_millis: u64::decode(dec)?,
        })
    }
}

/// Digest-keyed action result cache (`store/results/`).
#[derive(Clone, Debug)]
pub struct ActionCache {
    root: PathBuf,
}

impl ActionCache {
    /// Opens the cache inside an existing store root.
    pub fn open(cas: &Cas) -> io::Result<Self> {
        let root = cas.root().join("results");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path(&self, action: Digest) -> PathBuf {
        let hex = action.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    /// Looks up a cached result by action digest.
    pub fn get(&self, action: Digest) -> io::Result<Option<CachedResult>> {
        let path = self.path(action);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let result = canonical::decode_all::<CachedResult>(&bytes)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        Ok(Some(result))
    }

    /// Commits a successful result. Atomic and idempotent.
    pub fn put(&self, action: Digest, result: &CachedResult) -> io::Result<()> {
        let bytes = canonical::encode_vec(result);
        let path = self.path(action);
        fs::create_dir_all(path.parent().unwrap())?;
        let tmp = self.root.join(format!("tmp-{}", std::process::id()));
        fs::write(&tmp, &bytes)?;
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(err) if path.exists() => {
                let _ = fs::remove_file(&tmp);
                let _ = err;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tong_core::digest::Hasher;

    #[test]
    fn cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path().join("store")).unwrap();
        let cache = ActionCache::open(&cas).unwrap();

        let action = Hasher::digest(b"action");
        assert_eq!(cache.get(action).unwrap(), None);

        let result = CachedResult {
            outputs: TreeDigest::new(Hasher::digest(b"outputs")),
            stdout: BlobDigest::new(Hasher::digest(b"stdout")),
            stderr: BlobDigest::new(Hasher::digest(b"stderr")),
            duration_millis: 42,
        };
        cache.put(action, &result).unwrap();
        assert_eq!(cache.get(action).unwrap(), Some(result));
    }
}
