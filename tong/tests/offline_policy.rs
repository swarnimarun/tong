//! Offline-policy regressions through the real `tong` binary.
//!
//! - `offline_commands_never_fetch`: `--offline`/`--locked`/`--frozen`
//!   builds and fetches fail with a targeted missing-lock/missing-source
//!   diagnostic and never auto-lock, auto-fetch, or download — proven by
//!   failing against a reachable `file://` registry (a download attempt
//!   would succeed).
//! - `no_cache_actions_bypass_action_cache`: Cargo-imported test runs are
//!   `CachePolicy::NoCache`; they must re-execute on every run and must
//!   never be inserted into the action cache.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tong_core::digest::Hasher;

/// A crate published to the fixture registry.
struct FixtureCrate {
    name: &'static str,
    version: &'static str,
    lib: &'static str,
}

fn make_crate_archive(dir: &Path, fixture: &FixtureCrate) -> String {
    let top = format!("{}-{}", fixture.name, fixture.version);
    let build = dir.join("build").join(&top);
    fs::create_dir_all(build.join("src")).unwrap();
    fs::write(
        build.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"{}\"\nedition = \"2021\"\n",
            fixture.name, fixture.version
        ),
    )
    .unwrap();
    fs::write(build.join("src/lib.rs"), fixture.lib).unwrap();

    let archive_path = dir
        .join("crates")
        .join(fixture.name)
        .join(format!("{top}.crate"));
    fs::create_dir_all(archive_path.parent().unwrap()).unwrap();
    let file = fs::File::create(&archive_path).unwrap();
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    tar.append_dir_all(&top, &build).unwrap();
    let gz = tar.into_inner().unwrap();
    gz.finish().unwrap();

    let bytes = fs::read(&archive_path).unwrap();
    Hasher::digest(&bytes).to_hex()
}

fn write_index_entry(dir: &Path, fixture: &FixtureCrate, checksum: &str) {
    let name = fixture.name.to_ascii_lowercase();
    let path = match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{name}"),
        _ => format!("{}/{}/{}", &name[..2], &name[2..4], name),
    };
    let full = dir.join("index").join(&path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    let line = format!(
        "{{\"name\":\"{}\",\"vers\":\"{}\",\"deps\":[],\"cksum\":\"{}\",\"features\":{{}},\
         \"yanked\":false,\"v\":1}}\n",
        fixture.name, fixture.version, checksum
    );
    let mut existing = fs::read_to_string(&full).unwrap_or_default();
    existing.push_str(&line);
    fs::write(&full, existing).unwrap();
}

/// Builds a fixture registry (one crate `alpha`) and a workspace
/// depending on it; returns (registry, workspace, registry_root).
fn setup() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let registry = tempfile::tempdir().unwrap();
    let root = registry.path();
    let alpha = FixtureCrate {
        name: "alpha",
        version: "1.0.0",
        lib: "pub fn alpha() -> u32 { 7 }\n",
    };
    let checksum = make_crate_archive(root, &alpha);
    write_index_entry(root, &alpha, &checksum);
    fs::write(
        root.join("index").join("config.json"),
        format!(
            "{{\"dl\":\"file://{}/crates/{{crate}}/{{crate}}-{{version}}.crate\",\"api\":null}}\n",
            root.display()
        ),
    )
    .unwrap();

    let workspace = tempfile::tempdir().unwrap();
    let ws = workspace.path();
    fs::write(
        ws.join("Cargo.toml"),
        r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
alpha = "1"
"#,
    )
    .unwrap();
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(
        ws.join("src/main.rs"),
        "fn main() { println!(\"sum={}\", alpha::alpha()); }\n",
    )
    .unwrap();
    let root = root.to_path_buf();
    (registry, workspace, root)
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
    let mut command = Command::new(tong());
    command
        .args(args)
        .current_dir(workspace)
        .env("TONG_RUSTC", rustc_path());
    command.output().unwrap()
}

fn run_tong_registry(
    workspace: &Path,
    registry_index: &Path,
    args: &[&str],
) -> std::process::Output {
    let mut command = Command::new(tong());
    command
        .args(args)
        .current_dir(workspace)
        .env(
            "TONG_REGISTRY_INDEX",
            format!("file://{}", registry_index.display()),
        )
        .env("TONG_RUSTC", rustc_path());
    command.output().unwrap()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Recursively lists files under `root` as sorted workspace-relative
/// paths; used to prove the action cache gained (or did not gain) entries.
fn result_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    fn walk(dir: &Path, base: &Path, files: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, files);
            } else {
                files.push(
                    path.strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    walk(root, root, &mut files);
    files.sort();
    files
}

#[test]
fn offline_commands_never_fetch() {
    let (_registry, workspace, registry_root) = setup();
    let ws = workspace.path();
    let index = registry_root.join("index");

    // No lock yet: every offline/locked/frozen form fails with a targeted
    // diagnostic and never auto-locks (Tong.lock must stay absent).
    for args in [
        &["build", "--offline"][..],
        &["build", "--locked"][..],
        &["build", "--frozen"][..],
        &["fetch", "--offline"][..],
    ] {
        let output = run_tong_registry(ws, &index, args);
        assert!(
            !output.status.success(),
            "`tong {}` must fail without a lock: {}{}",
            args.join(" "),
            stdout_of(&output),
            stderr_of(&output)
        );
        assert!(
            stderr_of(&output).contains("no Tong.lock"),
            "expected a missing-lock diagnostic, got: {}",
            stderr_of(&output)
        );
        assert!(
            !ws.join("Tong.lock").exists(),
            "offline/locked commands must never auto-lock"
        );
    }

    // Populate the lock and sources online (file:// registry).
    let output = run_tong_registry(ws, &index, &["lock"]);
    assert!(
        output.status.success(),
        "lock failed: {}",
        stderr_of(&output)
    );
    let output = run_tong_registry(ws, &index, &["fetch"]);
    assert!(
        output.status.success(),
        "fetch failed: {}",
        stderr_of(&output)
    );

    // Delete the stored source archive: an offline build must fail naming
    // the missing source, and an offline fetch must refuse to download.
    // The file:// registry is reachable, so any network attempt would
    // succeed — failure is the proof that no download happened.
    let sources_dir = ws.join(".tong").join("store").join("sources");
    let blob = fs::read_dir(&sources_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "crate"))
        .expect("a stored .crate blob");
    fs::remove_file(&blob).unwrap();

    let output = run_tong_registry(ws, &index, &["build", "--offline"]);
    assert!(
        !output.status.success(),
        "offline build must fail on a missing source: {}",
        stdout_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("sources not fetched") && stderr.contains("alpha"),
        "expected a missing-source diagnostic naming `alpha`, got: {stderr}"
    );

    let output = run_tong_registry(ws, &index, &["fetch", "--offline"]);
    assert!(
        !output.status.success(),
        "offline fetch must fail on a missing source: {}",
        stdout_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("sources not fetched") && stderr.contains("alpha"),
        "expected a missing-source diagnostic naming `alpha`, got: {stderr}"
    );

    // Restore, then prove the positive control: a fully offline locked
    // build succeeds end to end.
    let output = run_tong_registry(ws, &index, &["fetch"]);
    assert!(
        output.status.success(),
        "restore fetch failed: {}",
        stderr_of(&output)
    );
    let output = run_tong_registry(ws, &index, &["build", "--offline", "--locked"]);
    assert!(
        output.status.success(),
        "offline locked build failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
}

#[test]
fn offline_build_rejects_missing_pinned_toolchain() {
    // A `[toolchain.rust] kind = "dist"` version that was never fetched:
    // `prepare` must fail with the fetch remedy before any download, with
    // or without `--offline`.
    let native = tempfile::tempdir().unwrap();
    let ws = native.path();
    fs::write(
        ws.join("Tong.toml"),
        "[workspace]\nname = \"app\"\n\n[toolchain.rust]\nkind = \"dist\"\nversion = \"9.9.9\"\n",
    )
    .unwrap();
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(ws.join("src/main.rs"), "fn main() {}\n").unwrap();

    let output = run_tong(ws, &["build", "--offline"]);
    assert!(
        !output.status.success(),
        "offline build must fail for an unfetched pinned toolchain: {}",
        stdout_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("is not fetched") && stderr.contains("toolchain fetch"),
        "expected the toolchain-fetch remedy, got: {stderr}"
    );
}

#[test]
fn no_cache_actions_bypass_action_cache() {
    let workspace = tempfile::tempdir().unwrap();
    let ws = workspace.path();
    fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(
        ws.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n\
         #[cfg(test)]\nmod tests {\n    \
         #[test]\n    fn adds() { assert_eq!(super::add(2, 2), 4); }\n}\n",
    )
    .unwrap();

    // First run: everything executes.
    let first = run_tong(ws, &["test"]);
    assert!(
        first.status.success(),
        "first test run failed: {}{}",
        stdout_of(&first),
        stderr_of(&first)
    );
    let first_stdout = stdout_of(&first);
    assert!(
        first_stdout.contains("rust:test-run:app:app"),
        "expected the test-run action, got: {first_stdout}"
    );
    assert!(first_stdout.contains("1 passed"), "{first_stdout}");

    // The compile actions' results were cached on the first run.
    let results_root = ws.join(".tong").join("store").join("results");
    let before = result_files(&results_root);
    assert!(
        !before.is_empty(),
        "compile actions should populate the cache"
    );

    // Second run: compile actions hit the cache, but the NoCache
    // test-run action must execute again — and must insert nothing.
    let second = run_tong(ws, &["test"]);
    assert!(
        second.status.success(),
        "second test run failed: {}{}",
        stdout_of(&second),
        stderr_of(&second)
    );
    let second_stdout = stdout_of(&second);
    assert!(
        second_stdout.contains("[cached]"),
        "compile actions should cache-hit on the second run: {second_stdout}"
    );
    let test_run_line = second_stdout
        .lines()
        .find(|line| line.contains("rust:test-run:app:app"))
        .expect("the test-run action line");
    assert!(
        !test_run_line.contains("[cached]"),
        "NoCache test run must re-execute, got: {test_run_line}"
    );
    assert!(second_stdout.contains("1 passed"), "{second_stdout}");

    let after = result_files(&results_root);
    assert_eq!(
        before, after,
        "a NoCache test run must never be inserted into the action cache"
    );
}
