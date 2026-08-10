//! End-to-end git dependencies through the real `tong` binary: `tong lock`
//! pins a tag's commit and captures its tree, a moved tag never changes an
//! offline build, `tong update` re-resolves, and a corrupted stored tree
//! fails verification before analysis.
//!
//! Fixture repositories are built with the ambient `git` CLI (test
//! scaffolding); the transport under test is `tong_fetch::git` (gix for
//! `file://`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tong_core::digest::Hasher;

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

fn tong() -> &'static str {
    env!("CARGO_BIN_EXE_tong")
}

fn rustc_path() -> PathBuf {
    std::env::var_os("TONG_RUSTC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rustc"))
}

fn run_tong(workspace: &Path, args: &[&str]) -> std::process::Output {
    Command::new(tong())
        .args(args)
        .current_dir(workspace)
        .env("TONG_RUSTC", rustc_path())
        .output()
        .unwrap()
}

/// Builds the git dependency repo (two commits, tag `v1` at the second)
/// and a workspace depending on it. Returns (repo_tempdir, workspace,
/// commit_second, commit_third); the tempdirs keep the fixture alive.
fn setup() -> (tempfile::TempDir, tempfile::TempDir, String, String) {
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"gitdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn v() -> u32 { 1 }\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "first"]);
    fs::write(root.join("src/lib.rs"), "pub fn v() -> u32 { 2 }\n").unwrap();
    git(&root, &["commit", "-q", "-am", "second"]);
    let second = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["tag", "-m", "v1", "v1"]);

    let workspace = tempfile::tempdir().unwrap();
    let ws = workspace.path();
    fs::write(
        ws.join("Cargo.toml"),
        format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\ngitdep = {{ git = \"file://{}\", tag = \"v1\" }}\n",
            fs::canonicalize(&root).unwrap().display()
        ),
    )
    .unwrap();
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(
        ws.join("src/main.rs"),
        "fn main() { println!(\"v={}\", gitdep::v()); }\n",
    )
    .unwrap();
    // A third commit the tag will later move to.
    fs::write(root.join("src/lib.rs"), "pub fn v() -> u32 { 3 }\n").unwrap();
    git(&root, &["commit", "-q", "-am", "third"]);
    let third = git(&root, &["rev-parse", "HEAD"]);

    (repo, workspace, second, third)
}

fn lock_text(workspace: &Path) -> String {
    fs::read_to_string(workspace.join("Tong.lock")).unwrap()
}

#[test]
fn git_locked_tag_survives_moved_tag_offline() {
    let (repo, workspace, second, _third) = setup();
    let repo = repo.path().to_path_buf();
    let ws = workspace.path();

    // Lock pins the tag at the second commit with a captured tree.
    let output = run_tong(ws, &["lock"]);
    assert!(
        output.status.success(),
        "lock failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = lock_text(ws);
    let git_line = text
        .lines()
        .find(|line| line.contains("git+file://") && line.contains(&second[..12]))
        .expect("Tong.lock pins the git commit");
    assert!(
        git_line.contains(&second),
        "expected commit {second} in {git_line}"
    );

    // Fetch materializes the tree; an offline locked build compiles.
    let output = run_tong(ws, &["fetch"]);
    assert!(output.status.success(), "fetch failed");
    let output = run_tong(ws, &["build", "--offline", "--locked"]);
    assert!(
        output.status.success(),
        "offline build failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Move the tag to the third commit: `tong lock` must keep the locked
    // commit (moving content is rejected unless `tong update` runs).
    git(&repo, &["tag", "-f", "-m", "v1", "v1"]);
    let output = run_tong(ws, &["lock"]);
    assert!(
        output.status.success(),
        "re-lock failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = lock_text(ws);
    assert!(
        text.contains(&second),
        "the moved tag must not change the locked commit:\n{text}"
    );

    // The offline build still uses the original commit.
    let output = run_tong(ws, &["build", "--offline", "--locked"]);
    assert!(
        output.status.success(),
        "offline rebuild failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let run = run_tong(ws, &["run", ":app", "--offline", "--locked"]);
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.trim().ends_with("v=2"),
        "the locked (original) commit must be built, got: {stdout}"
    );
}

#[test]
fn git_update_resolves_moved_tag() {
    let (repo, workspace, _second, third) = setup();
    let repo = repo.path().to_path_buf();
    let ws = workspace.path();

    let output = run_tong(ws, &["lock"]);
    assert!(output.status.success(), "lock failed");

    // Move the tag and `tong update gitdep`: the lock moves to the new
    // commit.
    git(&repo, &["tag", "-f", "-m", "v1", "v1"]);
    let output = run_tong(ws, &["update", "gitdep"]);
    assert!(
        output.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = lock_text(ws);
    assert!(
        text.contains(&third),
        "update must re-resolve to the moved tag:\n{text}"
    );
}

#[test]
fn git_corrupt_stored_tree_fails_before_analysis() {
    let (_repo, workspace, _second, _third) = setup();
    let ws = workspace.path();

    let output = run_tong(ws, &["lock"]);
    assert!(output.status.success(), "lock failed");
    let output = run_tong(ws, &["fetch"]);
    assert!(output.status.success(), "fetch failed");
    let output = run_tong(ws, &["build", "--offline", "--locked"]);
    assert!(output.status.success(), "build failed");

    // Corrupt the stored blob holding the git package's lib.rs (its
    // content digest names the blob), then drop the materialized
    // checkout so the next build re-extracts the corrupted content.
    let lib_content = "pub fn v() -> u32 { 2 }\n";
    let blob_hex = Hasher::digest(lib_content.as_bytes()).to_hex();
    let store = ws.join(".tong").join("store");
    let blob_path = store
        .join("blobs")
        .join(&blob_hex[..2])
        .join(&blob_hex[2..]);
    assert!(
        blob_path.is_file(),
        "expected the lib.rs blob at {blob_path:?}"
    );
    // CAS blobs are immutable (0444); make it writable to simulate
    // on-disk corruption.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&blob_path).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&blob_path, perms).unwrap();
    }
    let mut bytes = fs::read(&blob_path).unwrap();
    bytes[0] ^= 0xff;
    fs::write(&blob_path, &bytes).unwrap();
    let checkouts = store.join("git");
    for entry in fs::read_dir(&checkouts).unwrap().flatten() {
        let dir = entry.path().join("checkouts");
        if dir.is_dir() {
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    // The offline locked build must fail with the verification diagnostic
    // before any analysis.
    let output = run_tong(ws, &["build", "--offline", "--locked"]);
    assert!(
        !output.status.success(),
        "a corrupted stored tree must fail the build"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed verification"),
        "expected the verification diagnostic, got: {stderr}"
    );
}
