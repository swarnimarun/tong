//! The docker-stage acceptance test (docs/docker-caching.md, PLAN.md §16):
//! a deps-only build from a manifests-only context, then a full build in
//! the same directory (the store flows forward, like docker layers),
//! asserting every dep action is a cache hit and only the workspace
//! binary executes.
//!
//! Runs the real `tong` binary against a hermetic `file://` registry:
//! `tong lock` in the repo → stage 1 (manifests + lockfile only):
//! `tong fetch` + `tong build --deps-only` → stage 2 (sources added):
//! `tong build` with dep actions cached. The workspace is a Cargo.toml
//! workspace (the mode the docker pattern targets).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tong_core::digest::Hasher;

/// A crate published to the fixture registry.
struct FixtureCrate {
    name: &'static str,
    version: &'static str,
    deps: Vec<(&'static str, &'static str)>,
    lib: &'static str,
}

fn make_crate_archive(dir: &Path, fixture: &FixtureCrate) -> String {
    let top = format!("{}-{}", fixture.name, fixture.version);
    let build = dir.join("build").join(&top);
    fs::create_dir_all(build.join("src")).unwrap();
    let deps = fixture
        .deps
        .iter()
        .map(|(dep, req)| format!("{dep} = \"{req}\""))
        .collect::<Vec<_>>()
        .join("\n");
    let deps = if deps.is_empty() {
        String::new()
    } else {
        format!("\n[dependencies]\n{deps}\n")
    };
    fs::write(
        build.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"{}\"\nedition = \"2021\"\n{deps}\n",
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
    let deps: Vec<String> = fixture
        .deps
        .iter()
        .map(|(dep, req)| {
            format!(
                "{{\"name\":\"{dep}\",\"req\":\"{req}\",\"features\":[],\"optional\":false,\
                 \"default_features\":true,\"target\":null,\"kind\":\"normal\",\"package\":null}}"
            )
        })
        .collect();
    let line = format!(
        "{{\"name\":\"{}\",\"vers\":\"{}\",\"deps\":[{}],\"cksum\":\"{}\",\"features\":{{}},\
         \"yanked\":false,\"v\":1}}\n",
        fixture.name,
        fixture.version,
        deps.join(","),
        checksum
    );
    let mut existing = fs::read_to_string(&full).unwrap_or_default();
    existing.push_str(&line);
    fs::write(&full, existing).unwrap();
}

/// Builds the fixture registry and a Cargo workspace depending on it. The
/// workspace root is a virtual manifest; `app` is the sole member.
fn setup() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let registry = tempfile::tempdir().unwrap();
    let root = registry.path();
    let crates = vec![
        FixtureCrate {
            name: "alpha",
            version: "1.0.0",
            deps: Vec::new(),
            lib: "pub fn alpha() -> u32 { 7 }\n",
        },
        FixtureCrate {
            name: "beta",
            version: "1.0.0",
            deps: vec![("alpha", "^1")],
            lib: "pub fn beta() -> u32 { alpha::alpha() + 1 }\n",
        },
        // `extra` is an optional dependency of `app` behind the non-default
        // feature `extra`: the lock must exclude it (feature-aware), and
        // the build must still succeed (inactive optional edges missing
        // from the lock are skipped).
        FixtureCrate {
            name: "extra",
            version: "1.0.0",
            deps: Vec::new(),
            lib: "pub fn extra() -> u32 { 99 }\n",
        },
    ];
    for fixture in &crates {
        let checksum = make_crate_archive(root, fixture);
        write_index_entry(root, fixture, &checksum);
    }
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
        "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::create_dir_all(ws.join("app/src")).unwrap();
    fs::write(
        ws.join("app/Cargo.toml"),
        r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
alpha = "1"
beta = "1"

[features]
extra = ["dep:extra"]
"#,
    )
    .unwrap();
    fs::write(
        ws.join("app/src/main.rs"),
        "fn main() { println!(\"sum={}\", alpha::alpha() + beta::beta()); }\n",
    )
    .unwrap();
    let registry_root = root.to_path_buf();
    (registry, workspace, registry_root)
}

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

fn run_tong(
    workspace: &Path,
    registry_index: Option<&Path>,
    args: &[&str],
) -> std::process::Output {
    let mut command = Command::new(tong());
    command.arg("-v").args(args).current_dir(workspace);
    if let Some(index) = registry_index {
        command.env("TONG_REGISTRY_INDEX", format!("file://{}", index.display()));
    }
    command.env("TONG_RUSTC", real_rustc()).output().unwrap()
}

#[test]
fn docker_stage_deps_only_then_full_build_hits_cache() {
    let (_registry, workspace, registry_root) = setup();
    let index = registry_root.join("index");
    let ws = workspace.path();

    // Repo-side: resolve and commit Tong.lock (docker never runs `tong
    // lock` — docs/docker-caching.md Feature 2).
    let output = run_tong(ws, Some(&index), &["lock", "--offline"]);
    assert!(
        output.status.success(),
        "lock failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(ws.join("Tong.lock").is_file());

    // Stage 1 (deps): only manifests + Tong.lock, no local sources. The
    // member manifest sits at its real relative path (design (b)).
    let stage = tempfile::tempdir().unwrap();
    let dir = stage.path();
    fs::copy(ws.join("Cargo.toml"), dir.join("Cargo.toml")).unwrap();
    fs::create_dir_all(dir.join("app")).unwrap();
    fs::copy(ws.join("app/Cargo.toml"), dir.join("app/Cargo.toml")).unwrap();
    fs::copy(ws.join("Tong.lock"), dir.join("Tong.lock")).unwrap();

    let output = run_tong(dir, Some(&index), &["fetch"]);
    assert!(
        output.status.success(),
        "fetch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = run_tong(dir, Some(&index), &["build", "--deps-only"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "deps-only build failed: {stdout}{stderr}"
    );
    // Dep actions executed; the workspace binary was not planned (the
    // manifests-only member has no targets) and nothing else ran.
    assert!(stderr.contains("rust:lib:alpha:rlib"), "{stderr}");
    assert!(stderr.contains("rust:lib:beta:rlib"), "{stderr}");
    assert!(!stderr.contains("rust:bin:app:app"), "{stderr}");
    assert!(!stderr.contains("[cached]"), "{stderr}");
    // Nothing assembled.
    assert!(
        !dir.join(".tong/out").exists(),
        "deps-only must not assemble"
    );

    // Image trim: `tong gc --older-than 0` after the deps build must not
    // remove anything the app stage needs (dep results are marked by the
    // deps manifest, local trees by the recorded source list).
    let output = run_tong(dir, Some(&index), &["gc", "--older-than", "0"]);
    assert!(
        output.status.success(),
        "gc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Stage 2 (app): the same directory plus the local sources — the
    // store flows forward exactly like a docker layer.
    fs::create_dir_all(dir.join("app/src")).unwrap();
    fs::copy(ws.join("app/src/main.rs"), dir.join("app/src/main.rs")).unwrap();

    let output = run_tong(dir, Some(&index), &["build"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "full build failed: {stdout}{stderr}"
    );
    // The workspace binary executed; both dep actions were cache hits.
    assert!(stderr.contains("rust:bin:app:app"), "{stderr}");
    assert!(
        stderr.contains("rust:lib:alpha:rlib (RustLibrary) [cached]"),
        "{stderr}"
    );
    assert!(
        stderr.contains("rust:lib:beta:rlib (RustLibrary) [cached]"),
        "{stderr}"
    );
    assert_eq!(stderr.matches("[cached]").count(), 2, "{stderr}");

    // The assembled binary works.
    let binary = dir.join(".tong/out/dev/app/app");
    assert!(binary.is_file(), "artifact missing: {}", binary.display());
    let run = Command::new(&binary).output().unwrap();
    assert!(run.status.success());
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "sum=15");
}

/// Native `Tong.toml` mode: every target is a workspace action, so
/// `--deps-only` plans the graph but executes nothing (the doc's
/// "toolchain capture only" behavior) and the later full build compiles
/// the workspace binary.
#[test]
fn deps_only_native_mode_skips_everything_then_full_build_works() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(
        dir.join("Tong.toml"),
        "schema = 1\n\n[target.app]\nrule = \"rust_binary\"\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("src/main.rs"),
        "fn main() { println!(\"native\"); }\n",
    )
    .unwrap();

    let output = run_tong(dir, None, &["build", "--deps-only"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "native deps-only failed: {stdout}{stderr}"
    );
    // All workspace actions skipped; nothing executed, nothing assembled.
    assert!(
        stdout.contains("build complete: 1 actions (0 cached, 0 executed, 1 skipped)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("deps-only: workspace actions skipped"),
        "{stdout}"
    );
    assert!(
        !dir.join(".tong/out").exists(),
        "deps-only must not assemble"
    );

    // The app stage runs the same directory: the binary compiles and runs.
    let output = run_tong(dir, None, &["build"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "native build failed: {stdout}{stderr}"
    );
    assert!(stderr.contains("rust:bin:app:app"), "{stderr}");
    let run = Command::new(dir.join(".tong/out/dev/app/app"))
        .output()
        .unwrap();
    assert!(run.status.success());
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "native");
}

/// GC directive (2026-08-09): never remove what current builds need — a
/// `--deps-only` build invalidates nothing, so its manifest must keep the
/// previous full build's closure as GC roots. Full → deps-only → hard GC
/// → full must be a total cache hit.
#[test]
fn deps_only_never_invalidates_the_local_cache() {
    let (_registry, workspace, registry_root) = setup();
    let index = registry_root.join("index");
    let ws = workspace.path();

    let output = run_tong(ws, Some(&index), &["lock", "--offline"]);
    assert!(output.status.success());

    // Docker pattern: fetch locked sources into the store.
    let output = run_tong(ws, Some(&index), &["fetch"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Full build: every action executes.
    let output = run_tong(ws, Some(&index), &["build"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "build failed: {stdout}{stderr}");
    assert_eq!(stderr.matches("[cached]").count(), 0, "{stderr}");

    // Deps-only build in the same store: dep actions are cache hits, the
    // workspace action is skipped — and the recorded manifest must keep
    // the full build's closure (results + sources) as GC roots.
    let output = run_tong(ws, Some(&index), &["build", "--deps-only"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "deps-only failed: {stdout}{stderr}"
    );
    assert_eq!(stderr.matches("[cached]").count(), 2, "{stderr}");
    assert!(stdout.contains("1 skipped"), "{stdout}");

    // Hard GC: unmarked objects die. Nothing the workspace references may
    // be unmarked at this point.
    let output = run_tong(ws, Some(&index), &["gc", "--older-than", "0"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Nothing the workspace references may have been unmarked: the gc
    // report must show zero deleted results.
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("deleted 0 results"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    // The next full build is a total cache hit: the deps-only build must
    // never orphan the local actions' results.
    let output = run_tong(ws, Some(&index), &["build"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "full build failed: {stdout}{stderr}"
    );
    assert_eq!(stderr.matches("[cached]").count(), 3, "{stderr}");
    assert!(stdout.contains("0 executed"), "{stdout}");
}
