//! Persistent system-toolchain capture cache tests
//! (docs/fingerprint-cache.md): restore across fresh stores, invalidation
//! on content and stat changes, corruption self-healing.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use tong_rust::{SystemRust, capture_system_rust};
use tong_store::Cas;

/// Serializes env-touching tests: `TONG_RUSTC`/`TONG_CACHE_DIR` are
/// process-global and this binary must not mutate them concurrently.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn real_rustc() -> PathBuf {
    let output = Command::new("rustc")
        .arg("--print")
        .arg("sysroot")
        .output()
        .expect("rustc must be available to run this test");
    assert!(output.status.success(), "rustc --print sysroot failed");
    let sysroot = String::from_utf8(output.stdout).expect("sysroot not UTF-8");
    PathBuf::from(sysroot.trim()).join("bin").join("rustc")
}

/// A fake sysroot. Capture stats and hashes its files; the wrapper answers
/// `--print sysroot` with this path, and `bin/rustc` must be runnable
/// because the capture-miss path queries it for `-vV` — a copy of the
/// real binary keeps the fake sysroot self-contained.
fn fake_sysroot(dir: &Path) -> PathBuf {
    let sysroot = dir.join("sysroot");
    fs::create_dir_all(sysroot.join("bin")).unwrap();
    fs::create_dir_all(sysroot.join("lib").join("rustlib")).unwrap();
    fs::copy(real_rustc(), sysroot.join("bin").join("rustc")).unwrap();
    fs::write(sysroot.join("bin").join("rust-lld"), b"fake lld").unwrap();
    fs::write(
        sysroot.join("lib").join("rustlib").join("libstd.rlib"),
        b"fake std",
    )
    .unwrap();
    fs::write(
        sysroot.join("lib").join("rustlib").join("libcore.rlib"),
        b"fake core",
    )
    .unwrap();
    sysroot
}

/// A rustc shim: answers `--print sysroot` with the fake sysroot and
/// passes everything else through to the real rustc (`-vV`).
fn rustc_wrapper(sysroot: &Path, dir: &Path) -> PathBuf {
    let wrapper = dir.join("rustc-wrapper.sh");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--print\" ] && [ \"$2\" = \"sysroot\" ]; then\n  echo \"{}\"\n  exit 0\nfi\nexec \"{}\" \"$@\"\n",
            sysroot.display(),
            real_rustc().display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    wrapper
}

fn capture(cas: &Cas, wrapper: &Path, cache_dir: &Path) -> SystemRust {
    // SAFETY: process-global env is touched only here, under ENV_LOCK, and
    // restored immediately after the call returns.
    unsafe {
        std::env::set_var("TONG_RUSTC", wrapper);
        std::env::set_var("TONG_CACHE_DIR", cache_dir);
    }
    let result = capture_system_rust(cas);
    unsafe {
        std::env::remove_var("TONG_RUSTC");
        std::env::remove_var("TONG_CACHE_DIR");
    }
    result.expect("capture must succeed")
}

/// The cache entry's manifest path (the single entry under the cache dir).
fn capture_manifest(cache_dir: &Path) -> PathBuf {
    let toolchains = cache_dir.join("toolchains");
    let entry = fs::read_dir(&toolchains)
        .expect("cache entry exists")
        .next()
        .expect("one cache entry")
        .unwrap();
    entry.path().join("capture")
}

fn inode(path: &Path) -> u64 {
    fs::metadata(path).unwrap().ino()
}

/// Corrupts the cached `lib_tree` object's schema-version byte.
fn corrupt_lib_tree_object(cache_dir: &Path) {
    let entry = fs::read_dir(cache_dir.join("toolchains"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let dir = entry.path();
    let manifest = fs::read_to_string(dir.join("capture")).unwrap();
    let hex = manifest
        .lines()
        .find_map(|line| line.strip_prefix("lib_tree "))
        .expect("lib_tree in manifest")
        .trim();
    let path = dir.join("objects").join("tree").join(hex);
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] ^= 0xFF;
    fs::write(&path, bytes).unwrap();
}

#[test]
fn capture_cache_reuse_invalidation_and_self_healing() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let sysroot = fake_sysroot(tmp.path());
    let wrapper = rustc_wrapper(&sysroot, tmp.path());
    let cache = tmp.path().join("cache");

    let cas1 = Cas::open(tmp.path().join("store1")).unwrap();
    let r1 = capture(&cas1, &wrapper, &cache);
    let manifest = capture_manifest(&cache);
    let manifest_ino = inode(&manifest);

    // Fresh store + unchanged sysroot: the cache must restore the exact
    // capture. An untouched manifest proves the hit (no re-capture/store).
    let cas2 = Cas::open(tmp.path().join("store2")).unwrap();
    let r2 = capture(&cas2, &wrapper, &cache);
    assert_eq!(r2.rustc_blob, r1.rustc_blob);
    assert_eq!(r2.sysroot_tree, r1.sysroot_tree);
    assert_eq!(r2.bundle.digest(), r1.bundle.digest());
    assert_eq!(inode(&manifest), manifest_ino, "expected a cache hit");
    // The restored objects are present in the fresh store.
    assert!(cas2.blob_path(r2.rustc_blob).is_some());

    // Content change invalidates and re-captures (manifest rewritten).
    fs::write(
        sysroot.join("lib").join("rustlib").join("libstd.rlib"),
        b"fake std v2",
    )
    .unwrap();
    let cas3 = Cas::open(tmp.path().join("store3")).unwrap();
    let r3 = capture(&cas3, &wrapper, &cache);
    assert_ne!(r3.sysroot_tree, r1.sysroot_tree);
    assert_ne!(r3.bundle.digest(), r1.bundle.digest());
    assert_ne!(inode(&manifest), manifest_ino);

    // Stat-only change (restoring the content bumps mtime): re-capture,
    // but the content digests come back identical (safe false-dirty).
    fs::write(
        sysroot.join("lib").join("rustlib").join("libstd.rlib"),
        b"fake std",
    )
    .unwrap();
    let cas4 = Cas::open(tmp.path().join("store4")).unwrap();
    let r4 = capture(&cas4, &wrapper, &cache);
    assert_eq!(r4.sysroot_tree, r1.sysroot_tree);
    assert_eq!(r4.bundle.digest(), r1.bundle.digest());

    // A corrupt cached object must self-heal by re-capturing.
    corrupt_lib_tree_object(&cache);
    let cas5 = Cas::open(tmp.path().join("store5")).unwrap();
    let r5 = capture(&cas5, &wrapper, &cache);
    assert_eq!(r5.sysroot_tree, r1.sysroot_tree);
    assert_eq!(r5.bundle.digest(), r1.bundle.digest());

    // A different cache dir is a miss, but the capture is still correct.
    let other_cache = tmp.path().join("other-cache");
    let cas6 = Cas::open(tmp.path().join("store6")).unwrap();
    let r6 = capture(&cas6, &wrapper, &other_cache);
    assert_eq!(r6.sysroot_tree, r1.sysroot_tree);
}
