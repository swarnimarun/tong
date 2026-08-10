//! Fixed-revision git sources: selector resolution, tree capture, locked
//! reuse, and corruption detection. Fixture repositories are built with
//! the ambient `git` CLI (test scaffolding only); the transport under test
//! is `tong_fetch::git` (gix for `file://`).

use std::fs;
use std::path::Path;
use std::process::Command;

use tong_core::artifact::TreeDigest;
use tong_core::digest::Hasher;
use tong_fetch::git::{LockedGit, materialize_tree, resolve_and_capture};
use tong_store::Cas;

fn git(repo: &Path, args: &[&str]) -> String {
    let mut full = vec!["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"];
    full.extend_from_slice(args);
    let output = Command::new("git")
        .args(&full)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git must be installed to build the fixture repo");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Creates a repository with two commits and a tag `v1` pointing at the
/// second; returns (tempdir, url, first_commit, second_commit).
fn make_repo() -> (tempfile::TempDir, String, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"gitdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn v() -> u32 { 1 }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "first"]);
    let first = git(root, &["rev-parse", "HEAD"]);
    fs::write(root.join("src/lib.rs"), "pub fn v() -> u32 { 2 }\n").unwrap();
    git(root, &["commit", "-q", "-am", "second"]);
    let second = git(root, &["rev-parse", "HEAD"]);
    git(root, &["tag", "-m", "v1", "v1"]);
    let url = format!("file://{}", fs::canonicalize(root).unwrap().display());
    (dir, url, first, second)
}

fn hex_tree(tree: &TreeDigest) -> String {
    tree.digest().to_hex()
}

#[test]
fn git_resolves_tag_and_captures_tree() {
    let (_repo, url, first, second) = make_repo();
    let store = tempfile::tempdir().unwrap();
    let cas = Cas::open(store.path()).unwrap();

    let resolved = resolve_and_capture(
        store.path(),
        &cas,
        &url,
        None,
        Some("v1"),
        None,
        false,
        None,
    )
    .unwrap();
    assert_eq!(resolved.commit, second);
    assert_ne!(resolved.commit, first);
    assert!(resolved.checkout.join("Cargo.toml").is_file());
    assert!(resolved.checkout.join("src/lib.rs").is_file());

    // The tree is in the CAS and re-captures to the same digest.
    let recaptured = cas.capture_dir(&resolved.checkout).unwrap();
    assert_eq!(hex_tree(&recaptured), hex_tree(&resolved.tree_digest));

    // A locked git reuses the exact commit without re-resolving.
    let locked = LockedGit {
        commit: second.clone(),
        tree_digest: resolved.tree_digest,
    };
    let again = resolve_and_capture(
        store.path(),
        &cas,
        &url,
        None,
        Some("v1"),
        None,
        true,
        Some(&locked),
    )
    .unwrap();
    assert_eq!(again.commit, second);
}

#[test]
fn git_moved_tag_rejected_unless_locked() {
    let (_repo, url, _first, second) = make_repo();
    let store = tempfile::tempdir().unwrap();
    let cas = Cas::open(store.path()).unwrap();

    // Lock tag v1 (points at `second`).
    let locked = {
        let resolved = resolve_and_capture(
            store.path(),
            &cas,
            &url,
            None,
            Some("v1"),
            None,
            false,
            None,
        )
        .unwrap();
        LockedGit {
            commit: resolved.commit.clone(),
            tree_digest: resolved.tree_digest,
        }
    };
    assert_eq!(locked.commit, second);

    // Move the tag to a new commit.
    let root_path = url.strip_prefix("file://").unwrap().to_owned();
    let root = Path::new(&root_path);
    fs::write(root.join("src/lib.rs"), "pub fn v() -> u32 { 3 }\n").unwrap();
    git(root, &["commit", "-q", "-am", "third"]);
    let third = git(root, &["rev-parse", "HEAD"]);
    git(root, &["tag", "-f", "-m", "v1", "v1"]);

    // A fresh resolution sees the moved tag…
    let fresh = resolve_and_capture(
        store.path(),
        &cas,
        &url,
        None,
        Some("v1"),
        None,
        false,
        None,
    )
    .unwrap();
    assert_eq!(fresh.commit, third);

    // …but the locked reuse keeps the original commit.
    let reused = resolve_and_capture(
        store.path(),
        &cas,
        &url,
        None,
        Some("v1"),
        None,
        true,
        Some(&locked),
    )
    .unwrap();
    assert_eq!(reused.commit, second);
    assert_ne!(reused.commit, third);
}

#[test]
fn git_locked_tree_materializes_without_network() {
    let (_repo, url, _first, second) = make_repo();
    let store = tempfile::tempdir().unwrap();
    let cas = Cas::open(store.path()).unwrap();

    let resolved = resolve_and_capture(
        store.path(),
        &cas,
        &url,
        None,
        Some("v1"),
        None,
        false,
        None,
    )
    .unwrap();
    let locked = LockedGit {
        commit: resolved.commit.clone(),
        tree_digest: resolved.tree_digest,
    };

    // Delete the checkout: locked reuse materializes from the CAS alone.
    fs::remove_dir_all(&resolved.checkout).unwrap();
    let reused = resolve_and_capture(
        store.path(),
        &cas,
        &url,
        None,
        Some("v1"),
        None,
        true,
        Some(&locked),
    )
    .unwrap();
    assert_eq!(reused.commit, second);
    assert!(reused.checkout.join("Cargo.toml").is_file());
}

#[test]
fn git_corrupt_tree_fails_verification() {
    let (_repo, url, _first, second) = make_repo();
    let store = tempfile::tempdir().unwrap();
    let cas = Cas::open(store.path()).unwrap();

    let resolved = resolve_and_capture(
        store.path(),
        &cas,
        &url,
        None,
        Some("v1"),
        None,
        false,
        None,
    )
    .unwrap();
    assert_eq!(resolved.commit, second);

    // Corrupt the materialized tree: modify a file, then re-materialize —
    // the digest verification must fail before analysis.
    let lib = resolved.checkout.join("src/lib.rs");
    fs::write(&lib, "pub fn v() -> u32 { 99 }\n").unwrap();
    let err = materialize_tree(&cas, resolved.tree_digest, store.path(), &url, &second)
        .expect_err("corrupted tree must fail verification");
    assert!(
        err.to_string().contains("failed verification"),
        "unexpected error: {err}"
    );
    assert!(
        !resolved.checkout.exists(),
        "the corrupted checkout must be removed"
    );

    // The stored blob itself is unaffected (content-addressed): an honest
    // re-capture of the pristine tree still matches.
    let pristine = resolve_and_capture(
        store.path(),
        &cas,
        &url,
        None,
        Some("v1"),
        None,
        false,
        None,
    )
    .unwrap();
    assert_eq!(pristine.commit, second);
    assert_eq!(
        cas.capture_dir(&pristine.checkout).unwrap(),
        pristine.tree_digest
    );
}

#[test]
fn git_rejects_submodules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"subs\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    // A `.gitmodules` file marks a submodule-using repository even without
    // a real gitlink entry.
    fs::write(
        root.join(".gitmodules"),
        "[submodule \"x\"]\n\tpath = x\n\turl = https://x\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "init"]);

    let url = format!("file://{}", fs::canonicalize(root).unwrap().display());
    let store = tempfile::tempdir().unwrap();
    let cas = Cas::open(store.path()).unwrap();
    let err = resolve_and_capture(store.path(), &cas, &url, None, None, None, false, None)
        .expect_err("submodules must be rejected");
    assert!(
        err.to_string().contains("submodules") && err.to_string().contains("Tong"),
        "unexpected error: {err}"
    );
}

#[test]
fn git_captured_tree_is_byte_identical_to_repo_content() {
    let (_repo, url, _first, second) = make_repo();
    let store = tempfile::tempdir().unwrap();
    let cas = Cas::open(store.path()).unwrap();

    let resolved = resolve_and_capture(
        store.path(),
        &cas,
        &url,
        None,
        Some("v1"),
        None,
        false,
        None,
    )
    .unwrap();
    assert_eq!(resolved.commit, second);
    // The lib content at the locked commit (second) is `2`.
    let lib = fs::read_to_string(resolved.checkout.join("src/lib.rs")).unwrap();
    assert_eq!(lib, "pub fn v() -> u32 { 2 }\n");
    // The captured tree digest is stable: same content → same digest.
    let expected = cas.capture_dir(&resolved.checkout).unwrap();
    assert_eq!(expected, resolved.tree_digest);
    let _ = Hasher::digest(b"unused");
}
