//! CLI-level tests for `tong dockerfile` (docs/docker-caching.md Feature
//! 3): the real binary writes `Dockerfile` + `.dockerignore` for a
//! Cargo.toml workspace and for a native `Tong.toml` workspace.

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

/// Writes a file, creating parent directories.
fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A Cargo workspace: members `app` (binary) and `libc` (library), a path
/// dependency `vendor/depc`, and a committed Cargo.lock.
fn cargo_workspace(dir: &Path) {
    write(
        &dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"libc\"]\nresolver = \"2\"\n",
    );
    write(&dir.join("Cargo.lock"), "# fixture lockfile\n");
    write(
        &dir.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndepc = { path = \"../vendor/depc\" }\n",
    );
    write(&dir.join("app/src/main.rs"), "fn main() {}\n");
    write(
        &dir.join("libc/Cargo.toml"),
        "[package]\nname = \"libc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&dir.join("libc/src/lib.rs"), "pub fn f() {}\n");
    write(
        &dir.join("vendor/depc/Cargo.toml"),
        "[package]\nname = \"depc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&dir.join("vendor/depc/src/lib.rs"), "pub fn g() {}\n");
}

#[test]
fn dockerfile_command_writes_cargo_workspace_dockerfile() {
    let tmp = tempfile::tempdir().unwrap();
    cargo_workspace(tmp.path());
    let out = tmp.path().join("docker");
    let output = run_tong(
        tmp.path(),
        &[
            "dockerfile",
            "--base",
            "rust:1.97-bookworm",
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "dockerfile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(out.join("Dockerfile")).unwrap();
    assert!(
        text.contains("FROM rust:1.97-bookworm AS toolchain"),
        "{text}"
    );
    assert!(text.contains("COPY Cargo.toml Cargo.lock ./"), "{text}");
    assert!(
        text.contains("COPY app/Cargo.toml app/Cargo.toml"),
        "{text}"
    );
    assert!(
        text.contains("COPY libc/Cargo.toml libc/Cargo.toml"),
        "{text}"
    );
    assert!(text.contains("COPY vendor/depc vendor/depc"), "{text}");
    assert!(
        text.contains("RUN tong build --deps-only --profile dev"),
        "{text}"
    );
    assert!(text.contains("RUN tong build --profile dev"), "{text}");
    // No Tong.lock in this fixture: fetch is not emitted.
    assert!(!text.contains("RUN tong fetch"), "{text}");
    assert!(
        text.contains("COPY --from=app /app/.tong/out/dev/app/app /usr/local/bin/app"),
        "{text}"
    );
    assert!(!text.contains("depc/depc"), "{text}");
    assert_eq!(
        fs::read_to_string(out.join(".dockerignore")).unwrap(),
        ".git/\n.tong/\ntarget/\n**/target/\n"
    );
}

#[test]
fn dockerfile_command_emits_fetch_when_lockfile_committed() {
    let tmp = tempfile::tempdir().unwrap();
    cargo_workspace(tmp.path());
    write(&tmp.path().join("Tong.lock"), "# fixture\n");
    let out = tmp.path().join("docker");
    let output = run_tong(
        tmp.path(),
        &[
            "dockerfile",
            "--base",
            "rust:1.97-bookworm",
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(out.join("Dockerfile")).unwrap();
    assert!(text.contains("COPY Tong.lock ./"), "{text}");
    assert!(text.contains("RUN tong fetch"), "{text}");
}

#[test]
fn dockerfile_command_native_pinned_version_defaults_base() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        &tmp.path().join("Tong.toml"),
        "schema = 1\n\n[toolchain.rust]\nkind = \"dist\"\nversion = \"1.90.0\"\n\n[target.app]\nrule = \"rust_binary\"\n",
    );
    let out = tmp.path().join("docker");
    let output = run_tong(
        tmp.path(),
        &["dockerfile", "--output", out.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "dockerfile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(out.join("Dockerfile")).unwrap();
    assert!(text.contains("FROM rust:1.90.0 AS toolchain"), "{text}");
    assert!(text.contains("COPY Tong.toml ./"), "{text}");
    assert!(!text.contains("Cargo.toml"), "{text}");
    assert!(!text.contains("RUN tong fetch"), "{text}");
}

#[test]
fn dockerfile_command_requires_base_without_pinned_version() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        &tmp.path().join("Tong.toml"),
        "schema = 1\n\n[target.app]\nrule = \"rust_binary\"\n",
    );
    let output = run_tong(tmp.path(), &["dockerfile"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--base"), "{stderr}");
}
