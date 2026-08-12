//! Crate archive downloading and extraction into the source store.
//!
//! `.crate` archives are stored as CAS blobs under `<store>/sources/`
//! (content-addressed; dedup is automatic) and extracted into
//! `<store>/sources/checkout/<name>-<version>-<cksum-short>/`. Downloads
//! verify the SHA-256 against the index checksum before anything is stored;
//! a checksum mismatch stores nothing. Extraction is idempotent per
//! checksum.

use std::fs;
use std::path::{Path, PathBuf};

use semver::Version;

use tong_core::artifact::TreeDigest;
use tong_core::digest::{DIGEST_HEX_LEN, Hasher};
use tong_store::Cas;

use crate::registry::{FetchError, RegistryConfig, fetch_crate_bytes};
use crate::resolve::ResolvedPackage;

const SOURCE_EXTRACTION_SCHEMA_VERSION: u32 = 1;

/// The `.crate` blob path for a checksum.
pub fn crate_blob_path(store: &Path, checksum: &str) -> PathBuf {
    store.join("sources").join(format!("{checksum}.crate"))
}

fn source_tree_path(store: &Path, checksum: &str) -> PathBuf {
    store.join("sources").join(format!(
        "{checksum}.tree-v{SOURCE_EXTRACTION_SCHEMA_VERSION}"
    ))
}

/// Reads the canonical source tree captured for a checksum-locked archive.
pub fn source_tree_digest(store: &Path, checksum: &str) -> Option<TreeDigest> {
    let text = fs::read_to_string(source_tree_path(store, checksum)).ok()?;
    tong_core::digest::Digest::from_hex(text.trim())
        .ok()
        .map(TreeDigest::new)
}

/// Atomically records the source tree produced by verified extraction.
pub fn record_source_tree(
    store: &Path,
    checksum: &str,
    tree: TreeDigest,
) -> Result<(), FetchError> {
    let path = source_tree_path(store, checksum);
    let parent = path.parent().expect("source tree sidecar parent");
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".tmp-tree-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    fs::write(&tmp, tree.digest().to_hex())?;
    match fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(err) if path.is_file() => {
            let _ = fs::remove_file(tmp);
            let _ = err;
            Ok(())
        }
        Err(err) => Err(FetchError::Io(err)),
    }
}

/// The extracted checkout directory for a package.
pub fn checkout_dir(store: &Path, name: &str, version: &Version, checksum: &str) -> PathBuf {
    let short = &checksum[..checksum.len().min(12)];
    store
        .join("sources")
        .join("checkout")
        .join(format!("{name}-{version}-{short}"))
}

/// Downloads (if needed) and extracts a resolved package, returning the
/// tree digest of its extracted source tree. Re-downloads nothing already
/// present.
pub fn fetch_crate(
    cas: &Cas,
    config: &mut RegistryConfig,
    pkg: &ResolvedPackage,
) -> Result<TreeDigest, FetchError> {
    let checksum = pkg
        .checksum
        .as_deref()
        .expect("registry packages always carry a checksum");
    let blob_path = crate_blob_path(cas.root(), checksum);
    if !blob_path.is_file() {
        // The download template comes from `<index>/config.json`; fetch
        // it lazily, only when a download is actually needed.
        config.ensure_configured()?;
        let url = config.download_url(&pkg.name, &pkg.version, checksum);
        let bytes = fetch_crate_bytes(&url)?;
        verify_crate_bytes(&bytes, checksum, &pkg.name)?;
        fs::create_dir_all(blob_path.parent().expect("sources dir"))?;
        fs::write(&blob_path, bytes)?;
    } else {
        // Content-addressing is only sound if the stored blob really has
        // the expected checksum; verify instead of trusting the path.
        verify_crate_bytes(&fs::read(&blob_path)?, checksum, &pkg.name)?;
    }

    let checkout = checkout_dir(cas.root(), &pkg.name, &pkg.version, checksum);
    let expected_tree = source_tree_digest(cas.root(), checksum);
    if checkout.is_dir()
        && expected_tree.is_some_and(|expected| {
            cas.capture_dir(&checkout)
                .is_ok_and(|captured| captured != expected)
        })
    {
        // A verified sidecar is authoritative. Repair a modified checkout
        // from the checksum-verified archive instead of blessing its current
        // contents with a replacement sidecar.
        fs::remove_dir_all(&checkout)?;
    }
    if !checkout.is_dir() {
        extract_crate(&blob_path, &checkout)?;
    }
    let tree = cas.capture_dir(&checkout).map_err(FetchError::Io)?;
    if let Some(expected) = expected_tree
        && tree != expected
    {
        return Err(FetchError::BadConfig(format!(
            "source tree for `{} {}` changed after verified extraction (expected {}, got {})",
            pkg.name,
            pkg.version,
            expected.digest(),
            tree.digest()
        )));
    }
    record_source_tree(cas.root(), checksum, tree)?;
    Ok(tree)
}

/// Verifies archive bytes against the expected index checksum.
pub fn verify_crate_bytes(bytes: &[u8], expected: &str, package: &str) -> Result<(), FetchError> {
    let got = Hasher::digest(bytes).to_hex();
    if got != expected {
        return Err(FetchError::Checksum {
            package: package.to_owned(),
            expected: expected.to_owned(),
            got,
        });
    }
    Ok(())
}

/// Materializes a locked package's checkout from the stored `.crate` blob
/// (no network). Errors when the blob is missing (call `tong fetch`).
pub fn materialize_source(
    store: &Path,
    name: &str,
    version: &Version,
    checksum: &str,
) -> Result<PathBuf, FetchError> {
    let blob_path = crate_blob_path(store, checksum);
    if !blob_path.is_file() {
        return Err(FetchError::NotFound(format!(
            "source archive for `{name} {version}` ({checksum}.crate)"
        )));
    }
    let checkout = checkout_dir(store, name, version, checksum);
    // A fetch-time tree sidecar is produced only after the archive checksum
    // was verified and extraction captured. Builds validate the checkout's
    // content tree against this digest, so re-reading every compressed
    // archive here adds I/O without adding integrity.
    if checkout.is_dir() && source_tree_digest(store, checksum).is_some() {
        return Ok(checkout);
    }
    verify_crate_bytes(&fs::read(&blob_path)?, checksum, name)?;
    if !checkout.is_dir() {
        extract_crate(&blob_path, &checkout)?;
    }
    Ok(checkout)
}

/// Extracts a gzipped tar `.crate` into `dest` (created if missing).
fn extract_crate(archive: &Path, dest: &Path) -> Result<(), FetchError> {
    let file = fs::File::open(archive)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    let parent = dest.parent().expect("checkout parent");
    fs::create_dir_all(parent)?;
    // The archive contains a single top-level dir `<name>-<version>/`;
    // extract to a temp dir and rename the inner dir into place, so a
    // partial extraction never leaves a half-valid checkout.
    let tmp = parent.join(format!(".tmp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    tar.unpack(&tmp).map_err(|err| {
        let _ = fs::remove_dir_all(&tmp);
        FetchError::Io(err)
    })?;
    let inner = fs::read_dir(&tmp)?
        .filter_map(|entry| entry.ok())
        .find(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .ok_or_else(|| {
            FetchError::BadConfig(format!(
                "{} contains no top-level directory",
                archive.display()
            ))
        })?;
    let inner = inner.path();
    let _ = fs::remove_dir_all(dest);
    match fs::rename(&inner, dest) {
        Ok(()) => {
            let _ = fs::remove_dir_all(&tmp);
            Ok(())
        }
        Err(err) => {
            let _ = fs::remove_dir_all(&tmp);
            Err(FetchError::Io(err))
        }
    }
}

/// Whether `text` looks like a sha256 hex digest.
pub fn is_sha256_hex(text: &str) -> bool {
    text.len() == DIGEST_HEX_LEN && text.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_crate(checksum: &str) -> (tempfile::TempDir, PathBuf) {
        // Build a gzipped tar whose sha256 is `checksum`: a single dir
        // `foo-1.0.0/` with `Cargo.toml` and `src/lib.rs`.
        let dir = tempfile::tempdir().unwrap();
        let build = dir.path().join("build");
        fs::create_dir_all(build.join("foo-1.0.0/src")).unwrap();
        fs::write(build.join("foo-1.0.0/Cargo.toml"), "[package]\n").unwrap();
        fs::write(build.join("foo-1.0.0/src/lib.rs"), "pub fn f() {}\n").unwrap();
        let tar_path = dir.path().join("foo.crate");
        let file = fs::File::create(&tar_path).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        tar.append_dir_all("foo-1.0.0", build.join("foo-1.0.0"))
            .unwrap();
        let gz = tar.into_inner().unwrap();
        gz.finish().unwrap();
        let bytes = fs::read(&tar_path).unwrap();
        let actual = Hasher::digest(&bytes).to_hex();
        let expected = if checksum.is_empty() {
            actual
        } else {
            checksum.to_owned()
        };
        let renamed = dir.path().join(format!("{expected}.crate"));
        fs::rename(&tar_path, &renamed).unwrap();
        (dir, renamed)
    }

    #[test]
    fn verify_accepts_matching_checksum() {
        let (dir, path) = make_crate("");
        let bytes = fs::read(&path).unwrap();
        let digest = Hasher::digest(&bytes).to_hex();
        assert!(verify_crate_bytes(&bytes, &digest, "foo").is_ok());
        let _ = dir;
    }

    #[test]
    fn verify_rejects_corrupted_archives() {
        let (dir, path) = make_crate("");
        let mut bytes = fs::read(&path).unwrap();
        let digest = Hasher::digest(&bytes).to_hex();
        // Flip a byte.
        bytes[10] ^= 0xff;
        let err = verify_crate_bytes(&bytes, &digest, "foo").unwrap_err();
        assert!(matches!(err, FetchError::Checksum { .. }), "{err}");
        let _ = dir;
    }

    #[test]
    fn extracts_and_captures_crates() {
        let (dir, path) = make_crate("");
        let store = dir.path().join("store");
        let cas = Cas::open(&store).unwrap();
        let blob = store.join("sources").join(path.file_name().unwrap());
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::copy(&path, &blob).unwrap();
        let checksum = path.file_name().unwrap().to_str().unwrap();
        let checksum = checksum.strip_suffix(".crate").unwrap();
        let pkg = ResolvedPackage {
            name: "foo".to_owned(),
            version: Version::new(1, 0, 0),
            source: None,
            checksum: Some(checksum.to_owned()),
            yanked: false,
            local: false,
            dependencies: Vec::new(),
        };
        let mut config = RegistryConfig::crates_io();
        let tree = fetch_crate(&cas, &mut config, &pkg).unwrap();
        let checkout = checkout_dir(&store, "foo", &pkg.version, checksum);
        assert!(checkout.join("Cargo.toml").is_file());
        assert!(checkout.join("src/lib.rs").is_file());
        assert_eq!(source_tree_digest(&store, checksum), Some(tree));
        assert!(
            store
                .join("sources")
                .join(format!("{checksum}.tree-v1"))
                .is_file()
        );
        // Idempotent: re-fetch reuses everything.
        let again = fetch_crate(&cas, &mut config, &pkg).unwrap();
        assert_eq!(tree, again);

        // A modified checkout is repaired from the verified archive without
        // replacing the canonical source-tree sidecar.
        fs::write(checkout.join("src/lib.rs"), "pub fn tampered() {}\n").unwrap();
        let repaired = fetch_crate(&cas, &mut config, &pkg).unwrap();
        assert_eq!(tree, repaired);
        assert_eq!(
            fs::read_to_string(checkout.join("src/lib.rs")).unwrap(),
            "pub fn f() {}\n"
        );
    }
}
