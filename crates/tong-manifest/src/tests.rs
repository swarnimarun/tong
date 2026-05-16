use super::*;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn loads_cargo_manifest_with_default_targets_and_path_dependency() {
    let root = temp_dir("manifest-cargo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "").unwrap();
    fs::write(root.join("src/main.rs"), "").unwrap();
    fs::write(
        root.join("Cargo.toml"),
        r#"
[package]
name = "demo-app"
version = "0.1.0"
edition = "2024"

[dependencies]
helper = { path = "helper", features = ["fast"], default-features = false }
"#,
    )
    .unwrap();

    let manifest = Manifest::load(&root.join("Cargo.toml")).unwrap();

    assert_eq!(manifest.package.name, "demo-app");
    assert_eq!(manifest.lib.as_ref().unwrap().name, "demo_app");
    assert_eq!(manifest.bins[0].name, "demo-app");
    assert_eq!(manifest.dependencies[0].alias, "helper");
    assert_eq!(manifest.dependencies[0].features, ["fast"]);
    assert!(!manifest.dependencies[0].default_features);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn loads_tong_manifest_source_overrides() {
    let root = temp_dir("manifest-tong");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/main.rs"), "").unwrap();
    fs::write(
        root.join("Tong.toml"),
        r#"
[package]
name = "demo"

[dependencies]
dep = "1.0.0"

[tong.sources.dep]
tar = "file://fixtures/dep.crate"
sha256 = "abc123"
strip-prefix = "dep-1.0.0"
"#,
    )
    .unwrap();

    let manifest = Manifest::load(&root.join("Tong.toml")).unwrap();

    assert_eq!(manifest.kind, ManifestKind::Tong);
    assert_eq!(manifest.package.version, "0.0.0");
    assert!(matches!(
        manifest.dependencies[0].source,
        DependencySource::Registry { ref version } if version == "1.0.0"
    ));
    assert!(matches!(
        manifest.sources.get("dep").unwrap(),
        SourceSpec::Tar { url, sha256, strip_prefix, .. }
            if url.ends_with("fixtures/dep.crate")
                && sha256.as_deref() == Some("abc123")
                && strip_prefix.as_deref() == Some("dep-1.0.0")
    ));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn loads_build_dependencies() {
    let root = temp_dir("manifest-build-deps");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "").unwrap();
    fs::write(
        root.join("Cargo.toml"),
        r#"
[package]
name = "build-dep-demo"
version = "0.1.0"
edition = "2021"

[build-dependencies]
cc = "1.0"
"#,
    )
    .unwrap();

    let manifest = Manifest::load(&root.join("Cargo.toml")).unwrap();
    assert_eq!(manifest.build_dependencies.len(), 1);
    assert_eq!(manifest.build_dependencies[0].alias, "cc");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn loads_test_and_example_targets() {
    let root = temp_dir("manifest-tests-examples");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "").unwrap();
    fs::write(
        root.join("Cargo.toml"),
        r#"
[package]
name = "test-example-demo"
version = "0.1.0"
edition = "2021"

[[test]]
name = "integration"
path = "tests/integration.rs"
required-features = ["integration"]

[[example]]
name = "demo"
path = "examples/demo.rs"
"#,
    )
    .unwrap();

    let manifest = Manifest::load(&root.join("Cargo.toml")).unwrap();
    assert_eq!(manifest.tests.len(), 1);
    assert_eq!(manifest.tests[0].name, "integration");
    assert_eq!(manifest.tests[0].required_features, ["integration"]);
    assert_eq!(manifest.examples.len(), 1);
    assert_eq!(manifest.examples[0].name, "demo");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn extends_performs_shallow_merge() {
    let root = temp_dir("manifest-extends");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "").unwrap();
    fs::write(
        root.join("Cargo.toml"),
        r#"
[package]
name = "base-app"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1.0"
"#,
    )
    .unwrap();
    fs::write(
        root.join("Tong.toml"),
        r#"
extends = "Cargo.toml"

[package]
version = "0.2.0-tong"

[dependencies]
tokio = "1.0"

[tong.sources.tokio]
tar = "file://fixtures/tokio.crate"
"#,
    )
    .unwrap();

    let manifest = Manifest::load(&root.join("Tong.toml")).unwrap();

    assert_eq!(manifest.package.name, "base-app");
    assert_eq!(manifest.package.version, "0.2.0-tong");
    assert_eq!(manifest.package.edition, "2021");
    assert_eq!(manifest.dependencies.len(), 2);
    let dep_names: Vec<_> = manifest.dependencies.iter().map(|d| &d.alias).collect();
    assert!(dep_names.contains(&&"serde".to_string()));
    assert!(dep_names.contains(&&"tokio".to_string()));
    assert!(manifest.sources.contains_key("tokio"));

    fs::remove_dir_all(root).unwrap();
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("tong-{name}-{}-{nanos}", std::process::id()))
}
