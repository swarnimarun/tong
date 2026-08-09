//! The differential suite: compare tong's Cargo import + feature
//! resolution against `cargo metadata` for a corpus of fixture workspaces.
//!
//! For every fixture: run `cargo metadata` (subprocess; dev machines have
//! cargo) and tong's import (`cargo_import` + `resolve_features`), then
//! assert the same package set + versions, the same per-package activated
//! feature sets, and the same dependency edges. The compatibility matrix is
//! printed at the end; unsupported fixtures assert a targeted diagnostic,
//! divergent fixtures assert the documented divergence.
//!
//! The registry fixture serves its index over a local HTTP server (cargo's
//! `file://` transports are unreliable) with `[source]` replacement, so
//! `cargo metadata` stays offline.

mod compat {
    pub mod http_server;
}

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use compat::http_server::serve_registry;
use serde_json::Value;
use tong_rust::{FeatureRequest, RustModel, import_cargo_workspace, resolve_features};
use tong_store::{ActionCache, Cas};

/// Fixture compatibility status.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    /// Package set, versions, features, and edges all match cargo.
    Pass,
    /// Tong fails with a targeted diagnostic (asserted).
    Unsupported,
    /// A documented difference (asserted against the documented shape).
    Divergent,
}

struct Fixture {
    name: &'static str,
    status: Status,
    reason: &'static str,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "features",
        status: Status::Pass,
        reason: "",
    },
    Fixture {
        name: "targets",
        status: Status::Divergent,
        reason: "target-specific deps are filtered at import (host only); cargo keeps them in the resolve graph for lockfile completeness",
    },
    Fixture {
        name: "tests",
        status: Status::Pass,
        reason: "",
    },
    Fixture {
        name: "build-script",
        status: Status::Pass,
        reason: "",
    },
    Fixture {
        name: "registry",
        status: Status::Pass,
        reason: "",
    },
    Fixture {
        name: "examples",
        status: Status::Unsupported,
        reason: "[[example]] targets are not supported in this wave",
    },
];

/// The provider used for provider-free fixtures: registry deps are
/// rejected with the targeted "requires Tong.lock" diagnostic.
struct NoLock;

impl tong_rust::LockedSourceProvider for NoLock {
    fn locked_version(
        &self,
        edge: &tong_rust::RegistryEdge,
    ) -> Result<Option<semver::Version>, tong_rust::CargoImportError> {
        Err(tong_rust::CargoImportError::Unsupported(format!(
            "registry dependency `{}` requires Tong.lock; run `tong lock`",
            edge.package
        )))
    }

    fn source_dir(
        &self,
        _name: &str,
        _version: &semver::Version,
    ) -> Result<PathBuf, tong_rust::CargoImportError> {
        Err(tong_rust::CargoImportError::Unsupported(
            "no locked source provider".to_owned(),
        ))
    }
}

/// The lockfile-backed provider (registry fixture): mirrors the driver's
/// `LockedSource`.
struct LockedSource {
    lock: tong_fetch::TongLock,
    store: PathBuf,
}

impl LockedSource {
    fn new(lock: tong_fetch::TongLock, store: PathBuf) -> Self {
        Self { lock, store }
    }
}

impl tong_rust::LockedSourceProvider for LockedSource {
    fn locked_version(
        &self,
        edge: &tong_rust::RegistryEdge,
    ) -> Result<Option<semver::Version>, tong_rust::CargoImportError> {
        let package = self.lock.package(&edge.package).ok_or_else(|| {
            tong_rust::CargoImportError::Unsupported(format!(
                "registry dependency `{}` is not in Tong.lock; run `tong lock`",
                edge.package
            ))
        })?;
        let req = semver::VersionReq::parse(&edge.req).map_err(|err| {
            tong_rust::CargoImportError::Unsupported(format!(
                "invalid version requirement {:?} for `{}`: {err}",
                edge.req, edge.package
            ))
        })?;
        if !req.matches(&package.version) {
            return Err(tong_rust::CargoImportError::Unsupported(format!(
                "lockfile out of date: `{}` requires {} but Tong.lock has {}",
                edge.package, edge.req, package.version
            )));
        }
        Ok(Some(package.version.clone()))
    }

    fn source_dir(
        &self,
        name: &str,
        version: &semver::Version,
    ) -> Result<PathBuf, tong_rust::CargoImportError> {
        let package = self.lock.package(name).ok_or_else(|| {
            tong_rust::CargoImportError::Unsupported(format!("`{name}` is not in Tong.lock"))
        })?;
        let checksum = package.checksum.as_deref().ok_or_else(|| {
            tong_rust::CargoImportError::Unsupported(format!("`{name}` has no checksum"))
        })?;
        tong_fetch::materialize_source(&self.store, name, version, checksum).map_err(|err| {
            tong_rust::CargoImportError::Unsupported(format!("{err}; run `tong fetch`"))
        })
    }
}

/// Copies a fixture directory to a fresh temp dir (keeps the repo pristine
/// and isolates per-test state).
fn fixture_copy(name: &str) -> tempfile::TempDir {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("compat")
        .join(name);
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

/// Runs `cargo metadata` and returns the parsed JSON. A fresh `CARGO_HOME`
/// isolates the run from stale index caches (the test registry's port can
/// collide with earlier experiments).
fn cargo_metadata(dir: &Path) -> Value {
    let cargo_home = tempfile::tempdir().expect("cargo home tempdir");
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        // Config discovery follows the CWD, not --manifest-path.
        .current_dir(dir)
        .env("CARGO_HOME", cargo_home.path())
        .output()
        .expect("cargo must be installed to run the differential suite");
    if !output.status.success() {
        panic!(
            "cargo metadata failed for {}:\n{}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).expect("cargo metadata JSON")
}

/// The tong-side view: package set, features, and edges.
struct TongView {
    packages: BTreeMap<String, String>,
    features: BTreeMap<String, Vec<String>>,
    edges: Vec<(String, String)>,
}

/// Computes the tong view: feature resolution (dev-deps included) over the
/// full imported package set (cargo's resolve includes inactive optional
/// deps as nodes — lockfile completeness — but only active edges).
fn tong_view(model: &RustModel, include_dev: bool) -> TongView {
    let requests: Vec<FeatureRequest> = model
        .members
        .iter()
        .map(|name| FeatureRequest {
            package: name.clone(),
            features: Vec::new(),
            default_features: true,
        })
        .collect();
    let map = resolve_features(model, &requests, include_dev).expect("feature resolution");

    let mut packages = BTreeMap::new();
    let mut features = BTreeMap::new();
    let mut edges = Vec::new();
    for pkg in &model.packages {
        packages.insert(pkg.name.clone(), pkg.version.clone());
        features.insert(
            pkg.name.clone(),
            map.packages
                .get(&pkg.name)
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default(),
        );
        let active: Vec<_> = pkg
            .deps
            .iter()
            .chain(pkg.build_deps.iter())
            .chain(if include_dev {
                pkg.dev_deps.iter()
            } else {
                [].iter()
            })
            .filter(|dep| {
                !dep.optional
                    || map
                        .active_optional_deps
                        .get(&pkg.name)
                        .is_some_and(|active| active.contains(&dep.extern_name))
            })
            .map(|dep| dep.package.clone())
            .collect();
        for dep in active {
            edges.push((pkg.name.clone(), dep));
        }
    }
    edges.sort();
    TongView {
        packages,
        features,
        edges,
    }
}

/// The cargo-side view from `cargo metadata`.
fn cargo_view(metadata: &Value) -> TongView {
    let mut packages = BTreeMap::new();
    let mut features = BTreeMap::new();
    let mut edges = Vec::new();
    let packages_json = metadata["packages"].as_array().expect("packages");
    let mut id_to_name: BTreeMap<String, String> = BTreeMap::new();
    for package in packages_json {
        let name = package["name"].as_str().unwrap().to_owned();
        let version = package["version"].as_str().unwrap().to_owned();
        id_to_name.insert(package["id"].as_str().unwrap().to_owned(), name.clone());
        packages.insert(name, version);
    }
    let nodes = metadata["resolve"]["nodes"].as_array().expect("nodes");
    for node in nodes {
        let name = id_to_name
            .get(node["id"].as_str().unwrap())
            .cloned()
            .unwrap_or_else(|| node["id"].as_str().unwrap().to_owned());
        let mut node_features: Vec<String> = node["features"]
            .as_array()
            .expect("node features")
            .iter()
            .map(|f| f.as_str().unwrap().to_owned())
            .collect();
        node_features.sort();
        features.insert(name.clone(), node_features);
        for dep in node["dependencies"].as_array().expect("deps") {
            if let Some(dep_name) = id_to_name.get(dep.as_str().unwrap()) {
                edges.push((name.clone(), dep_name.clone()));
            }
        }
    }
    edges.sort();
    TongView {
        packages,
        features,
        edges,
    }
}

fn assert_matches(tong: &TongView, cargo: &TongView, fixture: &Fixture) {
    assert_eq!(
        tong.packages, cargo.packages,
        "fixture {}: package set mismatch (tong vs cargo)",
        fixture.name
    );
    assert_eq!(
        tong.features, cargo.features,
        "fixture {}: feature sets mismatch (tong vs cargo)",
        fixture.name
    );
    assert_eq!(
        tong.edges, cargo.edges,
        "fixture {}: dependency edges mismatch (tong vs cargo)",
        fixture.name
    );
}

/// Builds the fixture registry (alpha 1.0.0, beta 1.0.0 with an alpha
/// dep) and returns (tempdir, index_dir, dl_root).
fn build_registry() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let crates = [
        ("alpha", "1.0.0", &[][..], "pub fn alpha() -> u32 { 7 }\n"),
        (
            "beta",
            "1.0.0",
            &[("alpha", "^1", false)][..],
            "pub fn beta() -> u32 { alpha::alpha() + 1 }\n",
        ),
    ];
    for (name, version, deps, lib) in crates {
        let top = format!("{name}-{version}");
        let build = root.join("build").join(&top);
        fs::create_dir_all(build.join("src")).unwrap();
        let dep_lines: String = deps
            .iter()
            .map(|(dep, req, optional)| {
                format!(
                    "{dep} = {{ version = \"{req}\"{0} }}\n",
                    if *optional { ", optional = true" } else { "" }
                )
            })
            .collect();
        fs::write(
            build.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n\n\
                 [dependencies]\n{dep_lines}"
            ),
        )
        .unwrap();
        fs::write(build.join("src/lib.rs"), lib).unwrap();
        let archive = root.join("crates").join(name).join(format!("{top}.crate"));
        fs::create_dir_all(archive.parent().unwrap()).unwrap();
        let file = fs::File::create(&archive).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        tar.append_dir_all(&top, &build).unwrap();
        let gz = tar.into_inner().unwrap();
        gz.finish().unwrap();
        let bytes = fs::read(&archive).unwrap();
        let checksum = tong_core::digest::Hasher::digest(&bytes).to_hex();
        let lower = name.to_ascii_lowercase();
        let index_path = format!("{}/{}/{}", &lower[..2], &lower[2..4], lower);
        let index_file = root.join("index").join(&index_path);
        fs::create_dir_all(index_file.parent().unwrap()).unwrap();
        let dep_json: Vec<String> = deps
            .iter()
            .map(|(dep, req, optional)| {
                format!(
                    "{{\"name\":\"{dep}\",\"req\":\"{req}\",\"features\":[],\"optional\":{optional},\
                     \"default_features\":true,\"target\":null,\"kind\":\"normal\",\"package\":null}}"
                )
            })
            .collect();
        let mut existing = fs::read_to_string(&index_file).unwrap_or_default();
        existing.push_str(&format!(
            "{{\"name\":\"{name}\",\"vers\":\"{version}\",\"deps\":[{}],\"cksum\":\"{checksum}\",\
             \"features\":{{}},\"yanked\":false,\"v\":1}}\n",
            dep_json.join(",")
        ));
        fs::write(&index_file, existing).unwrap();
    }
    let dl_root = root.join("crates");
    let dl = format!(
        "file://{}/{{crate}}/{{crate}}-{{version}}.crate",
        dl_root.display()
    );
    fs::write(
        root.join("index").join("config.json"),
        format!("{{\"dl\":\"{dl}\"}}"),
    )
    .unwrap();
    (dir, root.join("index"), dl_root)
}

#[test]
fn differential_suite() {
    println!("compatibility matrix:");
    println!("{:<14} {:<11} reason", "fixture", "status");
    for fixture in FIXTURES {
        match fixture.status {
            Status::Pass => {
                check_pass(fixture);
            }
            Status::Unsupported => {
                check_unsupported(fixture);
            }
            Status::Divergent => {
                check_divergent(fixture);
            }
        }
        println!(
            "{:<14} {:<11} {}",
            fixture.name,
            format!("{:?}", fixture.status),
            fixture.reason
        );
    }
}

fn check_pass(fixture: &Fixture) {
    let work = fixture_copy(fixture.name);
    let dir = work.path();
    let host = host_triple();

    let mut provider = NoLock;
    let _ = &mut provider;
    // Keeps the registry tempdir alive for the whole check (dropping it
    // would delete the served index before cargo runs).
    let mut _registry_guard: Option<tempfile::TempDir> = None;
    let model = match fixture.name {
        "registry" => {
            // Serve the registry over HTTP for cargo, and point tong at
            // the same index (file:// is fine for tong).
            let (registry_dir, index, dl_root) = build_registry();
            _registry_guard = Some(registry_dir);
            let base = serve_registry(&index);
            fs::create_dir_all(dir.join(".cargo")).unwrap();
            let config_text = compat::http_server::cargo_source_config(&base);
            fs::write(dir.join(".cargo/config.toml"), &config_text).unwrap();
            // Tong side: lock + fetch from the file:// index, then import
            // through a lock-backed provider.
            let store = dir.join(".tong").join("store");
            let cas = Cas::open(&store).unwrap();
            let _cache = ActionCache::open(&cas).unwrap();
            let config =
                tong_fetch::RegistryConfig::from_url(&format!("file://{}", index.display()))
                    .unwrap();
            let index_client = tong_fetch::IndexClient::new(store.join("index"), config.clone());
            let roots = [
                tong_fetch::ResolvedDep {
                    name: "alpha".to_owned(),
                    req: semver::VersionReq::parse("1").unwrap(),
                    features: Vec::new(),
                    optional: false,
                    default_features: true,
                    kind: tong_fetch::DepKind::Normal,
                    registry: None,
                },
                tong_fetch::ResolvedDep {
                    name: "beta".to_owned(),
                    req: semver::VersionReq::parse("1").unwrap(),
                    features: Vec::new(),
                    optional: false,
                    default_features: true,
                    kind: tong_fetch::DepKind::Normal,
                    registry: None,
                },
            ];
            let packages =
                tong_fetch::resolve(&index_client, &roots, &Default::default()).expect("resolve");
            let mut lock = tong_fetch::TongLock {
                version: tong_fetch::LOCKFILE_VERSION,
                packages: Vec::new(),
            };
            for package in &packages {
                let deps = package
                    .dependencies
                    .iter()
                    .map(|dep| {
                        let version = packages
                            .iter()
                            .find(|p| p.name == dep.name)
                            .map(|p| p.version.to_string())
                            .unwrap_or_else(|| dep.req.to_string());
                        format!("{} {version} registry+file", dep.name)
                    })
                    .collect();
                lock.packages.push(tong_fetch::LockedPackage {
                    name: package.name.clone(),
                    version: package.version.clone(),
                    source: "registry+file".to_owned(),
                    checksum: Some(package.checksum.clone()),
                    manifest_checksum: None,
                    yanked: package.yanked,
                    publish_time: None,
                    dependencies: deps,
                });
                tong_fetch::fetch_crate(&cas, &config, package).expect("fetch crate");
            }
            let provider = LockedSource::new(lock, store.clone());
            let _ = dl_root;
            import_cargo_workspace(dir, &host, &provider).expect("registry import")
        }
        _ => import_cargo_workspace(dir, &host, &NoLock).expect("import"),
    };

    let cargo = cargo_view(&cargo_metadata(dir));
    let tong = tong_view(&model, true);
    assert_matches(&tong, &cargo, fixture);
}

fn check_unsupported(fixture: &Fixture) {
    let work = fixture_copy(fixture.name);
    let dir = work.path();
    let host = host_triple();
    let err = import_cargo_workspace(dir, &host, &NoLock).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("example"),
        "fixture {}: expected a targeted [[example]] diagnostic, got: {message}",
        fixture.name
    );
}

fn check_divergent(fixture: &Fixture) {
    let work = fixture_copy(fixture.name);
    let dir = work.path();
    let host = host_triple();
    let model = import_cargo_workspace(dir, &host, &NoLock).expect("import");
    let tong = tong_view(&model, true);
    let cargo = cargo_view(&cargo_metadata(dir));
    match fixture.name {
        // Host-only target filtering: tong's edge set is a subset of
        // cargo's (which keeps all-platform edges); package set, versions,
        // and features still match.
        "targets" => {
            assert_eq!(
                tong.packages, cargo.packages,
                "package set must still match"
            );
            assert_eq!(tong.features, cargo.features, "features must still match");
            assert!(
                tong.edges.iter().all(|edge| cargo.edges.contains(edge)),
                "tong edges must be a subset of cargo's"
            );
            assert!(
                tong.edges.len() < cargo.edges.len(),
                "the host-only filtering divergence must actually differ"
            );
        }
        other => panic!("no divergence asserted for fixture {other:?}"),
    }
}

/// The host triple via rustc.
fn host_triple() -> String {
    let output = Command::new("rustc")
        .args(["-vV"])
        .output()
        .expect("rustc must be installed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .expect("rustc -vV host")
}
