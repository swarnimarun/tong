//! End-to-end registry flow through the real `tong` binary against a
//! hermetic `file://` registry: `tong lock` → `tong fetch` → `tong build`
//! → offline rebuild from the source store.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tong_core::digest::Hasher;

/// A crate published to the fixture registry.
struct FixtureCrate {
    name: &'static str,
    version: &'static str,
    deps: Vec<(&'static str, &'static str, bool)>,
    lib: &'static str,
    yanked: bool,
}

fn make_crate_archive(dir: &Path, fixture: &FixtureCrate) -> String {
    let top = format!("{}-{}", fixture.name, fixture.version);
    let build = dir.join("build").join(&top);
    fs::create_dir_all(build.join("src")).unwrap();
    let deps = fixture
        .deps
        .iter()
        .map(|(dep, req, optional)| {
            format!(
                "{dep} = {{ version = \"{req}\"{0} }}",
                if *optional { ", optional = true" } else { "" }
            )
        })
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
            "[package]\nname = \"{}\"\nversion = \"{}\"\nedition = \"2021\"\n{deps}",
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
        .map(|(dep, req, optional)| {
            format!(
                "{{\"name\":\"{dep}\",\"req\":\"{req}\",\"features\":[],\"optional\":{optional},\
                 \"default_features\":true,\"target\":null,\"kind\":\"normal\",\"package\":null}}"
            )
        })
        .collect();
    let line = format!(
        "{{\"name\":\"{}\",\"vers\":\"{}\",\"deps\":[{}],\"cksum\":\"{}\",\"features\":{{}},\
         \"yanked\":{},\"v\":1}}\n",
        fixture.name,
        fixture.version,
        deps.join(","),
        checksum,
        fixture.yanked
    );
    let mut existing = fs::read_to_string(&full).unwrap_or_default();
    existing.push_str(&line);
    fs::write(&full, existing).unwrap();
}

/// Builds the fixture registry and a workspace depending on it; returns
/// (registry_root, workspace_dir).
fn setup() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let registry = tempfile::tempdir().unwrap();
    let root = registry.path();
    let crates = vec![
        FixtureCrate {
            name: "alpha",
            version: "1.0.0",
            deps: Vec::new(),
            lib: "pub fn alpha() -> u32 { 7 }\n",
            yanked: false,
        },
        FixtureCrate {
            name: "beta",
            version: "1.0.0",
            deps: vec![("alpha", "^1", false)],
            lib: "pub fn beta() -> u32 { alpha::alpha() + 1 }\n",
            yanked: false,
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
        r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
alpha = "1"
beta = "1"
"#,
    )
    .unwrap();
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(
        ws.join("src/main.rs"),
        "fn main() { println!(\"sum={}\", alpha::alpha() + beta::beta()); }\n",
    )
    .unwrap();
    let root = root.to_path_buf();
    (registry, workspace, root)
}

fn tong() -> &'static str {
    env!("CARGO_BIN_EXE_tong")
}

fn run_tong(workspace: &Path, registry_index: &Path, args: &[&str]) -> std::process::Output {
    Command::new(tong())
        .args(args)
        .current_dir(workspace)
        .env(
            "TONG_REGISTRY_INDEX",
            format!("file://{}", registry_index.display()),
        )
        .env("TONG_RUSTC", "/Users/swarnim/.cargo/bin/rustc")
        .output()
        .unwrap()
}

#[test]
fn registry_end_to_end_offline_build() {
    let (_registry, workspace, registry_root) = setup();
    let index = registry_root.join("index");
    let ws = workspace.path();

    // `tong build` from a clean state: the build auto-runs `tong lock`
    // (cargo generates Cargo.lock the same way) and `tong fetch` (sources
    // arrive on demand), logging both, and completes.
    let output = run_tong(ws, &index, &["build"]);
    assert!(
        output.status.success(),
        "build failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no Tong.lock — running `tong lock` first"),
        "expected the auto-lock notice, got: {stdout}"
    );
    assert!(ws.join("Tong.lock").is_file(), "auto-lock wrote Tong.lock");

    // `tong lock --offline` (file:// index, no network) writes Tong.lock.
    let output = run_tong(ws, &index, &["lock", "--offline"]);
    assert!(
        output.status.success(),
        "lock failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(ws.join("Tong.lock").is_file());
    let lock_text = fs::read_to_string(ws.join("Tong.lock")).unwrap();
    assert!(lock_text.contains("alpha"), "{lock_text}");
    assert!(lock_text.contains("beta"), "{lock_text}");

    // `tong fetch` populates the source store.
    let output = run_tong(ws, &index, &["fetch", "--offline"]);
    assert!(
        output.status.success(),
        "fetch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = ws.join(".tong").join("store");
    let sources = store.join("sources");
    assert!(
        sources.join("checkout").is_dir(),
        "no checkouts after fetch"
    );
    assert!(
        fs::read_dir(sources.join("checkout")).unwrap().count() >= 2,
        "expected alpha + beta checkouts"
    );

    // `tong build` compiles and links the registry deps.
    let output = run_tong(ws, &index, &["build"]);
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = run_tong(ws, &index, &["run", ":app"]);
    assert!(
        output.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim().ends_with("sum=15"),
        "expected the program output, got: {stdout}"
    );

    // Delete the extracted checkouts: the next build re-materializes them
    // from the store, offline.
    fs::remove_dir_all(sources.join("checkout")).unwrap();
    let output = run_tong(ws, &index, &["build"]);
    assert!(
        output.status.success(),
        "rebuild from store failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = run_tong(ws, &index, &["run", ":app"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim().ends_with("sum=15"),
        "expected the program output, got: {stdout}"
    );

    // Corrupt a stored .crate blob: `tong fetch` rejects it.
    let blob = fs::read_dir(sources)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "crate"))
        .expect("a stored .crate blob");
    let mut bytes = fs::read(&blob).unwrap();
    bytes[0] ^= 0xff;
    fs::write(&blob, &bytes).unwrap();
    let output = run_tong(ws, &index, &["fetch", "--offline"]);
    assert!(!output.status.success(), "corrupt blob must fail fetch");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("checksum"),
        "expected a checksum error, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
