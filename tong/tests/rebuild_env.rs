//! Build-script `rerun-if-env-changed` regression (2026-08-09): an env var
//! that is *unset* in the host environment must stay absent from the
//! script's run environment — injecting it as `""` breaks scripts that
//! fall back to a default with `unwrap_or` (e.g. `SDL3_DIR` → a Homebrew
//! prefix) on every rebuild after the first.
//!
//! Builds the fixture twice with the var unset: the second build must be a
//! total cache hit, proving the run input is stable.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tong() -> &'static str {
    env!("CARGO_BIN_EXE_tong")
}

fn real_rustc() -> String {
    let output = Command::new("rustc")
        .arg("--print")
        .arg("sysroot")
        .output()
        .expect("rustc must be available to run this test");
    assert!(output.status.success());
    let sysroot = String::from_utf8(output.stdout).expect("sysroot not UTF-8");
    PathBuf::from(sysroot.trim())
        .join("bin")
        .join("rustc")
        .to_string_lossy()
        .into_owned()
}

fn run_tong(workspace: &Path, args: &[&str]) -> std::process::Output {
    Command::new(tong())
        .args(args)
        .current_dir(workspace)
        .env("TONG_RUSTC", real_rustc())
        .output()
        .unwrap()
}

#[test]
fn unset_rerun_env_var_stays_absent_on_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = [\"app\"]\n").unwrap();
    fs::create_dir_all(dir.join("app/src")).unwrap();
    fs::write(
        dir.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         build = \"build.rs\"\n",
    )
    .unwrap();
    // The script mirrors the real-world `SDL3_DIR` pattern: an explicit
    // default, an explicit `unwrap_or`, and a panic on a present-but-empty
    // value (which is what the old `""` injection produced).
    fs::write(
        dir.join("app/build.rs"),
        "fn main() {\n\
         \x20   if let Ok(value) = std::env::var(\"MY_PREFIX\") {\n\
         \x20       if value.is_empty() {\n\
         \x20           panic!(\"MY_PREFIX present but empty\");\n\
         \x20       }\n\
         \x20   }\n\
         \x20   println!(\"cargo:rerun-if-env-changed=MY_PREFIX\");\n\
         \x20   println!(\"cargo:rerun-if-changed=build.rs\");\n\
         }\n",
    )
    .unwrap();
    fs::write(dir.join("app/src/main.rs"), "fn main() {}\n").unwrap();

    // First build: no previous manifest, the script runs with MY_PREFIX
    // absent and its fallback path succeeds.
    let output = run_tong(dir, &["build"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "first build failed: {stdout}{stderr}"
    );
    assert!(stdout.contains("rust:bs-run:app"), "{stdout}");

    // Second build, same environment: the previous manifest's
    // rerun-if-env-changed directive must not inject MY_PREFIX=\"\" — the
    // script's fallback keeps working. (The script re-runs once here: its
    // input narrows from the full tree to the declared rerun-if-changed
    // paths — the documented narrowing.)
    let output = run_tong(dir, &["build"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "rebuild failed: {stdout}{stderr}");

    // Third build: narrowed inputs are stable now — a total cache hit.
    let output = run_tong(dir, &["build"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "third build failed: {stdout}{stderr}"
    );
    assert_eq!(stdout.matches("[cached]").count(), 3, "{stdout}");
    assert!(stdout.contains("0 executed"), "{stdout}");
}
