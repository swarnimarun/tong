use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn tong() -> &'static str {
    env!("CARGO_BIN_EXE_tong")
}

fn rustc_path() -> PathBuf {
    std::env::var_os("TONG_RUSTC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rustc"))
}

fn write_workspace(root: &Path, tag: &str, sleep_ms: u64) {
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for name in ["a", "b"] {
        let package = root.join(name);
        fs::create_dir_all(package.join("src")).unwrap();
        fs::write(
            package.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}-{tag}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
            ),
        )
        .unwrap();
        fs::write(package.join("src/lib.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
        fs::write(
            package.join("build.rs"),
            format!(
                r#"fn main() {{
    let flags = std::env::var("CARGO_MAKEFLAGS").expect("Tong must export its jobserver");
    assert!(flags.contains("--jobserver-auth="), "{{flags}}");
    std::thread::sleep(std::time::Duration::from_millis({sleep_ms}));
    println!("cargo:rerun-if-changed=build.rs");
}}
// {tag}-{name}
"#
            ),
        )
        .unwrap();
    }
}

fn timed_build(root: &Path, store: &Path, jobs: usize) -> (Duration, std::process::Output) {
    let started = Instant::now();
    let output = Command::new(tong())
        .args([
            "--store-dir",
            store.to_str().unwrap(),
            "build",
            "--workspace",
            "--jobs",
            &jobs.to_string(),
        ])
        .current_dir(root)
        .env("TONG_RUSTC", rustc_path())
        .output()
        .unwrap();
    (started.elapsed(), output)
}

#[test]
fn jobs_run_independent_actions_in_parallel_and_export_jobserver() {
    let store = tempfile::tempdir().unwrap();
    let serial = tempfile::tempdir().unwrap();
    let parallel = tempfile::tempdir().unwrap();
    write_workspace(serial.path(), "serial", 1_000);
    write_workspace(parallel.path(), "parallel", 1_000);

    let (serial_duration, serial_output) = timed_build(serial.path(), store.path(), 1);
    assert!(
        serial_output.status.success(),
        "serial build failed: {}",
        String::from_utf8_lossy(&serial_output.stderr)
    );
    let (parallel_duration, parallel_output) = timed_build(parallel.path(), store.path(), 2);
    assert!(
        parallel_output.status.success(),
        "parallel build failed: {}",
        String::from_utf8_lossy(&parallel_output.stderr)
    );
    assert!(
        parallel_duration + Duration::from_millis(500) < serial_duration,
        "-j2 should overlap the independent one-second build scripts: serial={serial_duration:?}, parallel={parallel_duration:?}"
    );

    let log = Command::new(tong())
        .args([
            "--store-dir",
            store.path().to_str().unwrap(),
            "log",
            "--format",
            "json",
        ])
        .current_dir(parallel.path())
        .output()
        .unwrap();
    assert!(log.status.success());
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&log.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!events.is_empty());
    for event in events {
        for field in [
            "queue_wait_ms",
            "cache_lookup_ms",
            "execution_ms",
            "publication_ms",
            "total_duration_ms",
        ] {
            assert!(event[field].is_u64(), "missing {field}: {event}");
        }
    }
}

#[test]
fn jobs_rejects_zero() {
    let workspace = tempfile::tempdir().unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"zero-jobs\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(workspace.path().join("src")).unwrap();
    fs::write(workspace.path().join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    let output = Command::new(tong())
        .args(["build", "-j", "0"])
        .current_dir(workspace.path())
        .env("TONG_RUSTC", rustc_path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--jobs must be at least 1"));
}

#[test]
fn failure_stops_admission_but_drains_and_publishes_running_successes() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"bad\", \"slow\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for name in ["bad", "slow"] {
        let package = root.join(name);
        fs::create_dir_all(package.join("src")).unwrap();
        fs::write(
            package.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        fs::write(package.join("src/lib.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
    }
    let slow_started = root.join("slow-started");
    fs::write(
        root.join("slow/build.rs"),
        format!(
            "fn main() {{ std::fs::write({slow_started:?}, b\"started\").unwrap(); \
             std::thread::sleep(std::time::Duration::from_millis(1000)); }}\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join("bad/build.rs"),
        format!(
            "fn main() {{ for _ in 0..200 {{ if std::path::Path::new({slow_started:?}).exists() \
             {{ panic!(\"expected failure\"); }} std::thread::sleep(std::time::Duration::from_millis(25)); }} \
             panic!(\"slow action never started\"); }}\n"
        ),
    )
    .unwrap();

    let failed = Command::new(tong())
        .args([
            "--message-format",
            "json",
            "build",
            "--workspace",
            "-j",
            "2",
        ])
        .current_dir(root)
        .env("TONG_RUSTC", rustc_path())
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stdout).contains("\"type\":\"action-blocked\""),
        "failed descendants were not marked blocked: {}",
        String::from_utf8_lossy(&failed.stdout)
    );

    fs::write(root.join("bad/build.rs"), "fn main() {}\n").unwrap();
    let repaired = Command::new(tong())
        .args(["-v", "build", "--workspace", "-j", "2"])
        .current_dir(root)
        .env("TONG_RUSTC", rustc_path())
        .output()
        .unwrap();
    assert!(
        repaired.status.success(),
        "repaired build failed: {}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    let stderr = String::from_utf8_lossy(&repaired.stderr);
    assert!(
        stderr.contains("rust:bs-run:slow (RustBuildScriptRun) [cached]"),
        "the successful action running beside the failure was not published: {stderr}"
    );
}
