//! Native `Tong.toml` workspace behaviors through the real `tong` binary:
//! cross-member optional feature activation, test targets, escape and
//! unknown-rule rejection, and digest-stable label renames.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tong() -> &'static str {
    env!("CARGO_BIN_EXE_tong")
}

fn rustc_path() -> PathBuf {
    std::env::var_os("TONG_RUSTC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rustc"))
}

fn run_tong(workspace: &Path, store: Option<&Path>, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(tong());
    command
        .args(args)
        .current_dir(workspace)
        .env("TONG_RUSTC", rustc_path());
    if let Some(store) = store {
        command.env("TONG_STORE_DIR", store);
    }
    command.output().unwrap()
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Copies the native workspace fixture to a fresh temp dir.
fn fixture_copy() -> tempfile::TempDir {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("examples")
        .join("08-native-workspace");
    let target = tempfile::tempdir().unwrap();
    copy_dir(&source, target.path());
    target
}

fn copy_dir(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let dest = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

#[test]
fn native_workspace_builds_and_activates_cross_member_features() {
    let work = fixture_copy();
    let ws = work.path();

    // `--target web-app` builds the app binary; its dep table activates
    // core's optional `extra` feature, which compiles and links the
    // cross-member `extra` library.
    let output = run_tong(ws, None, &["build", "//app:app"]);
    assert!(
        output.status.success(),
        "build failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let run = run_tong(ws, None, &["run", ":web-app"]);
    assert!(run.status.success(), "run failed: {}", stderr_of(&run));
    assert!(
        stdout_of(&run).trim().ends_with("sum=10"),
        "optional cross-member feature must link extra, got: {}",
        stdout_of(&run)
    );

    // The member test target compiles and runs.
    let output = run_tong(ws, None, &["test", "//tests:integration"]);
    assert!(
        output.status.success(),
        "test failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).contains("1 passed"),
        "expected the integration test to pass: {}",
        stdout_of(&output)
    );
}

#[test]
fn native_workspace_rejects_escapes_and_unknown_rules() {
    let work = fixture_copy();
    let ws = work.path();

    // An escaping crate_root is rejected before planning.
    let app = ws.join("app/Tong.toml");
    let text = fs::read_to_string(&app).unwrap();
    let text = text.replace(
        "deps = [{ label = \"//core:core\", alias = \"core\", features = [\"extra\"] }]",
        "crate_root = \"../..\"\ndeps = [{ label = \"//core:core\", alias = \"core\", features = [\"extra\"] }]",
    );
    fs::write(&app, text).unwrap();
    let output = run_tong(ws, None, &["build"]);
    assert!(!output.status.success(), "escaping crate_root must fail");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("escapes the package root"),
        "expected the escape diagnostic, got: {stderr}"
    );

    // An unknown rule is a hard error listing the supported rules.
    let work = fixture_copy();
    let ws = work.path();
    let app = ws.join("app/Tong.toml");
    let text = fs::read_to_string(&app)
        .unwrap()
        .replace("rule = \"rust_binary\"", "rule = \"rust_wizard\"");
    fs::write(&app, text).unwrap();
    let output = run_tong(ws, None, &["build"]);
    assert!(!output.status.success(), "unknown rule must fail");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("supported rules:") && stderr.contains("rust_binary"),
        "expected the supported-rules diagnostic, got: {stderr}"
    );
}

#[test]
fn native_label_rename_is_digest_stable() {
    // Two workspaces, one shared store: the first builds `app` (stable
    // package_name/crate_name/output_name); the second renames only the
    // `[target.app]` table key. The compile actions must cache-hit —
    // renaming a graph label never changes the semantic digest.
    let store = tempfile::tempdir().unwrap();
    let store_path = store.path();

    let first = fixture_copy();
    let ws = first.path();
    let output = run_tong(ws, Some(store_path), &["build", "//app:app"]);
    assert!(output.status.success(), "first build failed");

    let second = fixture_copy();
    let ws2 = second.path();
    let app = ws2.join("app/Tong.toml");
    let text = fs::read_to_string(&app).unwrap();
    // Rename only the table key; the stable fields stay.
    let text = text.replace("[target.app]", "[target.webapp]");
    fs::write(&app, text).unwrap();
    // The `app` key no longer exists; build the whole workspace so the
    // renamed target still compiles against the shared store.
    let output = run_tong(ws2, Some(store_path), &["build"]);
    assert!(
        output.status.success(),
        "renamed build failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("[cached]"),
        "renaming the label must not invalidate compile actions: {stdout}"
    );
    assert!(
        stdout.contains("rust:bin:web-app:web-app"),
        "the package identity stays stable: {stdout}"
    );

    // The artifact keeps the declared output name and still runs.
    let binary = ws2.join(".tong/out/dev/web-app/web-app");
    assert!(binary.is_file(), "output_name must stay web-app");
    let run = Command::new(&binary).output().unwrap();
    assert!(
        String::from_utf8_lossy(&run.stdout)
            .trim()
            .ends_with("sum=10"),
        "renamed build must behave identically"
    );
}

#[test]
fn global_store_dir_shares_backing_between_worktrees() {
    let store = tempfile::tempdir().unwrap();
    let store_arg = store.path().to_str().unwrap();
    let first = fixture_copy();
    let second = fixture_copy();
    for workspace in [first.path(), second.path()] {
        let copied_build_state = workspace.join(".tong");
        if copied_build_state.exists() {
            fs::remove_dir_all(copied_build_state).unwrap();
        }
    }

    let output = run_tong(first.path(), None, &["build", "--store-dir", store_arg]);
    assert!(
        output.status.success(),
        "first shared build failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(!first.path().join(".tong/store").exists());
    assert!(
        first.path().join(".tong/out/dev/web-app/web-app").is_file(),
        "expected selected output: {}",
        stdout_of(&output)
    );

    let output = run_tong(second.path(), None, &["--store-dir", store_arg, "build"]);
    assert!(
        output.status.success(),
        "second shared build failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("[cached]"),
        "second worktree must reuse shared actions: {stdout}"
    );
    assert!(!second.path().join(".tong/store").exists());
    assert!(
        second
            .path()
            .join(".tong/out/dev/web-app/web-app")
            .is_file()
    );

    let output = run_tong(
        second.path(),
        None,
        &[
            "store",
            "path",
            "--format",
            "json",
            "--store-dir",
            store_arg,
        ],
    );
    assert!(output.status.success(), "store path failed");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema"], 1);
    assert_eq!(value["path"], store_arg);
}
