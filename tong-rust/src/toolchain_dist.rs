//! Downloaded Rust toolchain bundles (rustup dist protocol).
//!
//! `tong toolchain fetch rust --version <ver>` downloads the rustc and
//! rust-std components for the host triple from
//! `static.rust-lang.org/dist`, verifies their SHA-256 against the channel
//! manifest, extracts them (pure-Rust xz via `lzma-rs`), and captures the
//! result exactly like [`crate::toolchain::capture_system_rust`] — but with
//! a **portable** environment bundle (`tong.execution.portable=true`), so
//! the action digests carry the toolchain identity without host paths.
//!
//! Extraction is idempotent per version+triple; builds never touch the
//! network — a missing bundle is an actionable error naming the fetch
//! command (PLAN.md sections 5 and 9).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tong_core::action::CanonicalValue;
use tong_core::bundle::EnvironmentBundle;
use tong_core::digest::Hasher;
use tong_store::Cas;

use crate::toolchain::{SystemRust, ToolchainError, host_platform};

/// The rustup dist base URL.
pub const DIST_BASE: &str = "https://static.rust-lang.org/dist";

/// The extracted toolchain root for a version+triple.
pub fn dist_root(store: &Path, version: &str, triple: &str) -> PathBuf {
    store
        .join("toolchains")
        .join(format!("rust-{version}-{triple}"))
}

/// Fetches (or reuses) the dist toolchain for `version`/`triple` and
/// captures it into the store like a system toolchain.
///
/// `--target` triples other than the host are rejected (cross toolchains
/// are out of scope).
pub fn fetch_dist_rust(
    cas: &Cas,
    store: &Path,
    version: &str,
    triple: &str,
    host_triple: &str,
) -> Result<SystemRust, ToolchainError> {
    if triple != host_triple {
        return Err(ToolchainError::Dist(format!(
            "toolchain target {triple:?} differs from the host {host_triple:?}; \
             cross toolchains are not supported"
        )));
    }
    let root = dist_root(store, version, triple);
    if !root.join("rustc").join("bin").join("rustc").is_file() {
        download_dist(store, version, triple, &root)?;
    }
    capture_dist(cas, version, &root)
}

/// The dist channel manifest: the `pkg.<name>.target.<triple>.{url,hash}`
/// entries we need.
#[derive(serde::Deserialize, Default)]
struct ChannelManifest {
    pkg: std::collections::BTreeMap<String, ChannelPkg>,
}

#[derive(serde::Deserialize, Default)]
struct ChannelPkg {
    target: std::collections::BTreeMap<String, ChannelTarget>,
}

#[derive(serde::Deserialize, Default)]
struct ChannelTarget {
    url: Option<String>,
    hash: Option<String>,
}

/// Downloads and extracts the rustc + rust-std components.
fn download_dist(
    store: &Path,
    version: &str,
    triple: &str,
    root: &Path,
) -> Result<(), ToolchainError> {
    let manifest_url = format!("{DIST_BASE}/channel-rust-{version}.toml");
    println!("  fetching toolchain manifest {manifest_url}");
    let bytes = tong_fetch::fetch_url(&manifest_url, 16 * 1024 * 1024)
        .map_err(|err| ToolchainError::Dist(format!("cannot fetch {manifest_url}: {err}")))?;
    let manifest: ChannelManifest = toml::from_str(&String::from_utf8_lossy(&bytes))
        .map_err(|err| ToolchainError::Dist(format!("cannot parse channel manifest: {err}")))?;

    fs::create_dir_all(root)?;
    let tmp = store
        .join("toolchains")
        .join(format!(".tmp-{version}-{triple}"));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;

    for component in ["rustc", "rust-std"] {
        let entry = manifest
            .pkg
            .get(component)
            .and_then(|pkg| pkg.target.get(triple))
            .ok_or_else(|| {
                ToolchainError::Dist(format!(
                    "channel manifest has no {component} component for {triple}"
                ))
            })?;
        let url = entry
            .url
            .as_ref()
            .ok_or_else(|| ToolchainError::Dist(format!("no url for {component} {triple}")))?;
        let expected = entry
            .hash
            .as_deref()
            .unwrap_or("")
            .strip_prefix("sha256:")
            .unwrap_or("");
        let url = if url.starts_with('/') {
            format!("{DIST_BASE}{url}")
        } else {
            url.clone()
        };
        println!("  downloading {component} {version} ({triple})");
        let archive = tong_fetch::fetch_url(&url, 512 * 1024 * 1024)
            .map_err(|err| ToolchainError::Dist(format!("cannot download {url}: {err}")))?;
        if !expected.is_empty() {
            let got = Hasher::digest(&archive).to_hex();
            if got != expected {
                return Err(ToolchainError::Dist(format!(
                    "checksum mismatch for {component} {version}: expected {expected}, got {got}"
                )));
            }
        }
        // xz or gzip → tar → extract into a per-component dir.
        let tar_path = tmp.join(format!("{component}.tar"));
        let mut reader = archive.as_slice();
        let mut writer = fs::File::create(&tar_path)?;
        if url.ends_with(".tar.xz") {
            lzma_rs::xz_decompress(&mut reader, &mut writer)
                .map_err(|err| ToolchainError::Dist(format!("xz decompress failed: {err}")))?;
        } else if url.ends_with(".tar.gz") {
            let mut gz = flate2::read::GzDecoder::new(reader);
            std::io::copy(&mut gz, &mut writer)
                .map_err(|err| ToolchainError::Dist(format!("gzip decompress failed: {err}")))?;
        } else {
            return Err(ToolchainError::Dist(format!(
                "unsupported component archive format in {url}"
            )));
        }
        drop(writer);
        let mut tar = tar::Archive::new(fs::File::open(&tar_path)?);
        tar.unpack(&tmp).map_err(|err| {
            ToolchainError::Dist(format!("extract failed for {component}: {err}"))
        })?;
        let _ = fs::remove_file(&tar_path);
    }

    // The dist layout: rustc-<ver>-<triple>/rustc/{bin,lib,...} and
    // rust-std-<ver>-<triple>/rust-std-<triple>/lib — move the compiler
    // into place and merge the std into its sysroot (rustup's layout).
    let rustc_src = find_dir(&tmp, |path| {
        path.file_name().is_some_and(|name| name == "rustc")
            && path.parent().is_some_and(|parent| {
                parent
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("rustc-"))
            })
    })
    .ok_or_else(|| ToolchainError::Dist("rustc extraction layout unexpected".to_owned()))?;
    let merged = root.join("rustc");
    fs::rename(&rustc_src, &merged)
        .map_err(|err| ToolchainError::Dist(format!("cannot place rustc component: {err}")))?;
    let std_lib = find_dir(&tmp, |path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("rust-std-"))
            && path.ends_with("rust-std-".to_owned() + triple)
    })
    .ok_or_else(|| ToolchainError::Dist("rust-std extraction layout unexpected".to_owned()))?
    .join("lib");
    // The std's `lib/` contents (rustlib/<triple>/...) merge into the
    // compiler's `lib/` (rustup's install layout).
    let target_dir = merged.join("lib");
    fs::create_dir_all(&target_dir)?;
    copy_merge(&std_lib, &target_dir);
    let _ = fs::remove_dir_all(&tmp);
    Ok(())
}

fn find_dir(root: &Path, matches: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                if matches(&path) {
                    return Some(path);
                }
                stack.push(path);
            }
        }
    }
    None
}

fn copy_merge(source: &Path, dest: &Path) {
    let Ok(entries) = fs::read_dir(source) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let target = dest.join(entry.file_name());
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            fs::create_dir_all(&target).unwrap_or_default();
            copy_merge(&path, &target);
        } else if !target.exists() {
            let _ = fs::copy(&path, &target);
        }
    }
}

/// Captures an extracted dist toolchain like a system capture, but with a
/// portable bundle.
fn capture_dist(cas: &Cas, version: &str, root: &Path) -> Result<SystemRust, ToolchainError> {
    let real_rustc = root.join("rustc").join("bin").join("rustc");
    if !real_rustc.is_file() {
        return Err(ToolchainError::Dist(format!(
            "extracted toolchain at {} is incomplete (missing rustc)",
            root.display()
        )));
    }
    let version_verbose = crate::toolchain::run_toolchain(&real_rustc, &["-vV"])
        .map_err(|err| ToolchainError::Dist(format!("cannot run extracted rustc: {err}")))?;
    let host_triple = version_verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::trim)
        .ok_or_else(|| ToolchainError::Dist("extracted rustc reported no host".to_owned()))?
        .to_owned();

    let rustc_blob = cas.put_file(&real_rustc)?;
    let excludes = Default::default();
    let bin_tree = cas.fingerprint_dir(&root.join("rustc").join("bin"), &excludes)?;
    let lib_tree = cas.fingerprint_dir(&root.join("rustc").join("lib"), &excludes)?;
    let sysroot_tree = cas.assemble(&[
        (
            tong_core::paths::RelativePath::new("bin").unwrap(),
            bin_tree,
        ),
        (
            tong_core::paths::RelativePath::new("lib").unwrap(),
            lib_tree,
        ),
    ])?;

    let bundle = EnvironmentBundle {
        name: format!("rust-{version}-{host_triple}"),
        provider: "rustup-dist".to_owned(),
        platform: host_platform(),
        variables: BTreeMap::new(),
        files: sysroot_tree,
        metadata: BTreeMap::from([
            (
                "tong.rust.dist_version".to_owned(),
                CanonicalValue::String(version.to_owned()),
            ),
            (
                "tong.rust.rustc_verbose_version".to_owned(),
                CanonicalValue::String(version_verbose.trim().to_owned()),
            ),
            (
                "tong.rust.host_triple".to_owned(),
                CanonicalValue::String(host_triple.clone()),
            ),
            (
                "tong.execution.portable".to_owned(),
                CanonicalValue::String("true".to_owned()),
            ),
        ]),
    };
    cas.put_bundle(&bundle)?;

    Ok(SystemRust {
        root: root.join("rustc"),
        rustc: real_rustc,
        host_triple,
        version_verbose: version_verbose.trim().to_owned(),
        rustc_blob,
        sysroot_tree,
        bundle,
    })
}

/// Whether a dist toolchain for version/triple is already extracted.
pub fn dist_available(store: &Path, version: &str, triple: &str) -> bool {
    dist_root(store, version, triple)
        .join("rustc")
        .join("bin")
        .join("rustc")
        .is_file()
}

/// Loads an already-extracted dist toolchain without network access.
pub fn load_dist_rust(
    cas: &Cas,
    store: &Path,
    version: &str,
    triple: &str,
) -> Result<SystemRust, ToolchainError> {
    if !dist_available(store, version, triple) {
        return Err(ToolchainError::Dist(format!(
            "toolchain rust {version} ({triple}) is not fetched; \
             run `tong toolchain fetch rust --version {version}`"
        )));
    }
    capture_dist(cas, version, &dist_root(store, version, triple))
}
