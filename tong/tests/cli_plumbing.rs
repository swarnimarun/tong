//! Cargo-workflow CLI plumbing: the full command surface against one
//! native and one Cargo workspace, versioned JSON outputs, and
//! digest-based rebuild explanation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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

fn native_workspace() -> tempfile::TempDir {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("examples")
        .join("08-native-workspace");
    let target = tempfile::tempdir().unwrap();
    copy_dir(&source, target.path());
    target
}

/// A small Cargo workspace with a binary, a lib, and a test.
fn cargo_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/app\", \"crates/core\"]\ndefault-members = [\"crates/core\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::create_dir_all(ws.join("crates/app/src")).unwrap();
    fs::write(
        ws.join("crates/app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncore = { path = \"../core\" }\n",
    )
    .unwrap();
    fs::write(
        ws.join("crates/app/src/main.rs"),
        "fn main() { println!(\"{}\", core::v()); }\n",
    )
    .unwrap();
    fs::create_dir_all(ws.join("crates/app/tests")).unwrap();
    fs::write(
        ws.join("crates/app/tests/smoke.rs"),
        "#[test] fn smoke() {}\n",
    )
    .unwrap();
    fs::create_dir_all(ws.join("crates/app/examples")).unwrap();
    fs::write(ws.join("crates/app/examples/demo.rs"), "fn main() {}\n").unwrap();
    fs::create_dir_all(ws.join("crates/app/benches")).unwrap();
    fs::write(ws.join("crates/app/benches/simple.rs"), "fn main() {}\n").unwrap();
    fs::create_dir_all(ws.join("crates/core/src")).unwrap();
    fs::write(
        ws.join("crates/core/Cargo.toml"),
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        ws.join("crates/core/src/lib.rs"),
        "pub fn v() -> u32 { 7 }\n",
    )
    .unwrap();
    dir
}

fn assert_success(output: &std::process::Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed: {}{}",
        stdout_of(output),
        stderr_of(output)
    );
}

#[test]
fn shared_store_reuses_cargo_workspace_actions() {
    let store = tempfile::tempdir().unwrap();
    let store_arg = store.path().to_str().unwrap();
    let first = cargo_workspace();
    let second = cargo_workspace();

    let output = run_tong(
        first.path(),
        &["build", "--workspace", "--store-dir", store_arg],
    );
    assert_success(&output, "first shared Cargo build");
    assert!(!first.path().join(".tong/store").exists());

    let output = run_tong(
        second.path(),
        &["--store-dir", store_arg, "build", "--workspace"],
    );
    assert_success(&output, "second shared Cargo build");
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("[cached]"),
        "second Cargo worktree must reuse shared actions: {stdout}"
    );
    assert!(!second.path().join(".tong/store").exists());
}

#[test]
fn concurrent_workspace_build_waits_for_the_active_build() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("Cargo.toml"),
        "[package]\nname = \"serial-build\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(work.path().join("src")).unwrap();
    fs::write(
        work.path().join("src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    )
    .unwrap();
    fs::write(
        work.path().join("build.rs"),
        "fn main() { std::thread::sleep(std::time::Duration::from_millis(750)); }\n",
    )
    .unwrap();

    let first = Command::new(tong())
        .arg("build")
        .current_dir(work.path())
        .env("TONG_RUSTC", rustc_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lock_path = work.path().join(".tong/build.lock");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            && matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "first build never acquired its lock"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let second = run_tong(work.path(), &["build"]);
    let first = first.wait_with_output().unwrap();

    assert_success(&first, "first concurrent build");
    assert_success(&second, "waiting concurrent build");
    assert!(
        stderr_of(&second).contains("another build is running; waiting for it to finish"),
        "second build did not report waiting: {}",
        stderr_of(&second)
    );
}

#[test]
fn cargo_package_selection_materializes_different_bin_name() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("Cargo.toml"),
        r#"
[package]
name = "server"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "daemon"
path = "src/main.rs"
"#,
    )
    .unwrap();
    fs::create_dir_all(work.path().join("src")).unwrap();
    fs::write(work.path().join("src/main.rs"), "fn main() {}\n").unwrap();

    let output = run_tong(work.path(), &["build", "-p", "server"]);
    assert_success(&output, "build package with renamed binary");
    assert!(
        work.path().join(".tong/out/dev/daemon/daemon").is_file(),
        "package selection must materialize its binary: {}",
        stdout_of(&output)
    );
}

#[test]
fn cargo_workflow_commands_native_workspace() {
    let work = native_workspace();
    let ws = work.path();

    // Positional label selection builds the app.
    let output = run_tong(ws, &["build", "//app:app"]);
    assert_success(&output, "build //app:app");
    assert!(stdout_of(&output).contains("rust:bin:web-app:web-app"));

    // check builds metadata-only.
    let output = run_tong(ws, &["check", "//app:app"]);
    assert_success(&output, "check //app:app");
    assert!(stdout_of(&output).contains("rust:bin:web-app:web-app"));

    // run executes the binary with args after `--`.
    let output = run_tong(ws, &["run", "//app:app", "--", "ignored"]);
    assert_success(&output, "run //app:app");
    assert!(stdout_of(&output).trim().ends_with("sum=10"));

    // test compiles and runs the integration test; --no-run plans without
    // running.
    let output = run_tong(ws, &["test", "//tests:integration"]);
    assert_success(&output, "test //tests:integration");
    assert!(stdout_of(&output).contains("1 passed"));
    let output = run_tong(ws, &["test", "//tests:integration", "--no-run"]);
    assert_success(&output, "test --no-run");

    // bench builds the (empty) benchmark set.
    let output = run_tong(ws, &["bench"]);
    assert_success(&output, "bench");

    // Feature flags: -p selects packages by name.
    let output = run_tong(ws, &["build", "-p", "web-app"]);
    assert_success(&output, "build -p web-app");

    // query targets/deps/actions, JSON schema 1.
    let output = run_tong(ws, &["query", "targets", "--format", "json"]);
    assert_success(&output, "query targets json");
    let text = stdout_of(&output);
    assert!(text.contains("\"schema\":1"), "{text}");
    assert!(text.contains("web-app"), "{text}");
    assert!(text.contains("kind"), "{text}");
    let output = run_tong(ws, &["query", "deps", "//app:app", "--format", "json"]);
    assert_success(&output, "query deps json");
    let text = stdout_of(&output);
    assert!(text.contains("\"package\""), "{text}");
    assert!(text.contains("core"), "{text}");
    let output = run_tong(ws, &["query", "actions", "//app:app", "--format", "json"]);
    assert_success(&output, "query actions json");
    let text = stdout_of(&output);
    assert!(text.contains("\"logical_id\""), "{text}");
    assert!(text.contains("rust:bin:web-app:web-app"), "{text}");

    // graph JSON has schema 1 and nodes.
    let output = run_tong(ws, &["graph", "--format", "json"]);
    assert_success(&output, "graph json");
    let text = stdout_of(&output);
    assert!(text.contains("\"schema\":1"), "{text}");
    assert!(text.contains("\"nodes\""), "{text}");

    // log JSON records events after builds.
    let output = run_tong(ws, &["log", "--format", "json"]);
    assert_success(&output, "log json");
    let text = stdout_of(&output);
    assert!(text.contains("\"action\""), "{text}");
    assert!(text.contains("\"digest\""), "{text}");
}

#[test]
fn cargo_workflow_commands_cargo_workspace() {
    let work = cargo_workspace();
    let ws = work.path();

    let output = run_tong(ws, &["build", "-p", "app"]);
    assert_success(&output, "build -p app");
    let output = run_tong(ws, &["check"]);
    assert_success(&output, "check default-members");
    assert!(!stdout_of(&output).contains("rust:bin:app:app"));
    let output = run_tong(ws, &["check", "--workspace"]);
    assert_success(&output, "check --workspace");
    let output = run_tong(ws, &["test", "--workspace", "--no-run"]);
    assert_success(&output, "test --workspace --no-run");
    let output = run_tong(ws, &["bench", "--workspace", "--no-run"]);
    assert_success(&output, "bench --workspace --no-run");

    let output = run_tong(ws, &["check", "--workspace", "--exclude", "app"]);
    assert_success(&output, "check --workspace --exclude app");
    assert!(!stdout_of(&output).contains("rust:bin:app:app"));

    let runner = ws.join("runner");
    fs::create_dir(&runner).unwrap();
    let output = run_tong(
        &runner,
        &["check", "--manifest-path", "../Cargo.toml", "-p", "app"],
    );
    assert_success(&output, "check --manifest-path");
    for (selector, name, action) in [
        ("--bin", "app", "rust:bin:app:app"),
        ("--example", "demo", "rust:example:app:demo"),
        ("--test", "smoke", "rust:test-compile:app:test:smoke"),
        ("--bench", "simple", "rust:test-compile:app:bench:simple"),
    ] {
        let output = run_tong(ws, &["check", selector, name]);
        assert_success(&output, &format!("check {selector} {name}"));
        assert!(
            stdout_of(&output).contains(action),
            "{}",
            stdout_of(&output)
        );
    }
    let output = run_tong(ws, &["check", "--release", "-p", "app"]);
    assert_success(&output, "check --release");
    let output = run_tong(ws, &["run", "-p", "app", "--bin", "app"]);
    assert_success(&output, "run -p app --bin app");
    assert!(stdout_of(&output).trim().ends_with("7"));

    // Package specs: name, name@version, name@version#source.
    for spec in ["app", "app@0.1.0"] {
        let output = run_tong(ws, &["query", "targets", "-p", spec, "--format", "json"]);
        assert_success(&output, &format!("query -p {spec}"));
    }

    let output = run_tong(ws, &["query", "deps", "app", "--format", "json"]);
    assert_success(&output, "query deps app");
    assert!(
        stdout_of(&output).contains("core"),
        "{}",
        stdout_of(&output)
    );

    let output = run_tong(ws, &["graph", "--format", "dot"]);
    assert_success(&output, "graph dot");
    assert!(
        stdout_of(&output).contains("digraph"),
        "{}",
        stdout_of(&output)
    );

    let output = run_tong(ws, &["log", "--format", "text"]);
    assert_success(&output, "log text");
}

#[test]
fn explain_rebuild_reports_input_change() {
    let work = native_workspace();
    let ws = work.path();

    let output = run_tong(ws, &["build", "//app:app"]);
    assert_success(&output, "first build");

    // Change a source file the app depends on, then rebuild.
    fs::write(ws.join("extra/src/lib.rs"), "pub fn extra() -> u32 { 8 }\n").unwrap();
    let output = run_tong(ws, &["build", "//app:app"]);
    assert_success(&output, "second build");

    // The explanation names the changed input tree digest, not a generic
    // miss.
    let output = run_tong(ws, &["explain", "rebuild", "web-app"]);
    assert_success(&output, "explain rebuild");
    let text = stdout_of(&output);
    assert!(
        text.contains("changed input"),
        "expected the input-tree explanation, got: {text}"
    );
    assert!(
        text.contains("("),
        "the input digest should be reported: {text}"
    );
}
