//! Two versions of one crate must remain fully distinct: distinct
//! `PackageId`s, feature sets, action ids, and lock edges — while identical
//! inputs still deduplicate by digest.
//!
//! The fixture registry publishes `alpha` at 1.0.0 and 2.0.0 with
//! byte-identical library sources; the workspace depends on both through
//! renames (`alpha1 = { package = "alpha", version = "1" }` and
//! `alpha2 = { package = "alpha", version = "2" }`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tong_core::digest::Hasher;
use tong_rust::{
    FeatureRequest, PackageId, RustModel, SourceId, capture_system_rust, import_cargo_workspace,
    resolve_features,
};
use tong_store::Cas;

fn make_crate_archive(dir: &Path, name: &str, version: &str) -> String {
    let top = format!("{name}-{version}");
    let build = dir.join("build").join(&top);
    fs::create_dir_all(build.join("src")).unwrap();
    fs::write(
        build.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    // Byte-identical library source for both versions: only the manifest
    // version differs, so any identity leak into the action would alias.
    fs::write(build.join("src/lib.rs"), "pub fn alpha() -> u32 { 7 }\n").unwrap();

    let archive_path = dir.join("crates").join(name).join(format!("{top}.crate"));
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

fn write_index_entry(dir: &Path, name: &str, version: &str, checksum: &str) {
    let lower = name.to_ascii_lowercase();
    let path = format!("{}/{}/{}", &lower[..2], &lower[2..4], lower);
    let full = dir.join("index").join(&path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    let mut existing = fs::read_to_string(&full).unwrap_or_default();
    existing.push_str(&format!(
        "{{\"name\":\"{name}\",\"vers\":\"{version}\",\"deps\":[],\"cksum\":\"{checksum}\",\
         \"features\":{{}},\"yanked\":false,\"v\":1}}\n"
    ));
    fs::write(&full, existing).unwrap();
}

fn registry_id(version: &str) -> PackageId {
    PackageId {
        name: "alpha".to_owned(),
        version: semver::Version::parse(version).unwrap(),
        source: SourceId::Registry("fixture".to_owned()),
    }
}

/// Builds the fixture registry + workspace and returns the workspace
/// tempdir (kept alive) and the store path.
fn setup() -> (tempfile::TempDir, PathBuf) {
    let registry = tempfile::tempdir().unwrap();
    let root = registry.path();
    for version in ["1.0.0", "2.0.0"] {
        let checksum = make_crate_archive(root, "alpha", version);
        write_index_entry(root, "alpha", version, &checksum);
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
alpha1 = { package = "alpha", version = "=1.0.0" }
alpha2 = { package = "alpha", version = "=2.0.0" }
"#,
    )
    .unwrap();
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(
        ws.join("src/main.rs"),
        "fn main() { println!(\"{}\", alpha1::alpha() + alpha2::alpha()); }\n",
    )
    .unwrap();

    // Resolve both versions against the fixture index, lock them, and
    // fetch their checkouts into the store.
    let store = ws.join(".tong").join("store");
    let cas = Cas::open(&store).unwrap();
    let config =
        tong_fetch::RegistryConfig::from_url(&format!("file://{}", root.join("index").display()))
            .unwrap();
    let index_client = tong_fetch::IndexClient::new(store.join("index"), config.clone());
    let locals = [tong_fetch::LocalPackage {
        name: "app".to_owned(),
        version: semver::Version::new(0, 1, 0),
        source: Some("path+.".to_owned()),
        deps: vec![
            tong_fetch::ResolvedDep {
                name: "alpha".to_owned(),
                req: Some(semver::VersionReq::parse("=1.0.0").unwrap()),
                features: Vec::new(),
                optional: false,
                default_features: true,
                dev: false,
            },
            tong_fetch::ResolvedDep {
                name: "alpha".to_owned(),
                req: Some(semver::VersionReq::parse("=2.0.0").unwrap()),
                features: Vec::new(),
                optional: false,
                default_features: true,
                dev: false,
            },
        ],
    }];
    let packages = tong_fetch::resolve(&index_client, &locals, &Default::default()).unwrap();
    let mut lock = tong_fetch::TongLock {
        version: tong_fetch::LOCKFILE_VERSION,
        packages: Vec::new(),
    };
    for package in &packages {
        let deps: Vec<String> = package
            .dependencies
            .iter()
            .map(|(name, version, source)| {
                format!(
                    "{name} {version} {}",
                    source
                        .clone()
                        .unwrap_or_else(|| "registry+fixture".to_owned())
                )
            })
            .collect();
        lock.packages.push(tong_fetch::LockedPackage {
            name: package.name.clone(),
            version: package.version.clone(),
            source: package
                .source
                .clone()
                .unwrap_or_else(|| "registry+fixture".to_owned()),
            checksum: package.checksum.clone(),
            manifest_checksum: None,
            yanked: package.yanked,
            publish_time: None,
            dependencies: deps,
        });
        if !package.local {
            tong_fetch::fetch_crate(&cas, &config, package).unwrap();
        }
    }
    lock.save(ws).unwrap();
    drop(cas);
    (workspace, store)
}

/// The lockfile-backed provider: resolves each edge by requirement
/// matching (exact here — one candidate per version).
struct LockedFixture {
    lock: tong_fetch::TongLock,
    store: PathBuf,
}

impl LockedFixture {
    fn new(lock: tong_fetch::TongLock, store: PathBuf) -> Self {
        Self { lock, store }
    }
}

impl tong_rust::LockedSourceProvider for LockedFixture {
    fn locked_package(
        &self,
        edge: &tong_rust::RegistryEdge,
    ) -> Result<Option<tong_rust::LockedSource>, tong_rust::CargoImportError> {
        let req = semver::VersionReq::parse(&edge.req).map_err(|err| {
            tong_rust::CargoImportError::Unsupported(format!(
                "invalid version requirement {:?} for `{}`: {err}",
                edge.req, edge.package
            ))
        })?;
        let candidates: Vec<&tong_fetch::LockedPackage> = self
            .lock
            .candidates(&edge.package)
            .filter(|package| req.matches(&package.version))
            .collect();
        let package = match candidates.len() {
            1 => candidates[0].clone(),
            _ => {
                return Err(tong_rust::CargoImportError::Unsupported(format!(
                    "unexpected candidate count for `{}`: {}",
                    edge.package,
                    candidates.len()
                )));
            }
        };
        let checksum = package.checksum.as_deref().unwrap();
        let source_dir =
            tong_fetch::materialize_source(&self.store, &package.name, &package.version, checksum)
                .map_err(|err| {
                    tong_rust::CargoImportError::Unsupported(format!("{err}; run `tong fetch`"))
                })?;
        Ok(Some(tong_rust::LockedSource {
            id: registry_id(&package.version.to_string()),
            source_dir,
        }))
    }
}

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

/// The model with both alpha versions imported.
fn import_model(workspace: &Path, store: &Path) -> RustModel {
    let lock = tong_fetch::TongLock::load(workspace).unwrap();
    let provider = LockedFixture::new(lock, store.to_path_buf());
    import_cargo_workspace(workspace, &host_triple(), &provider).unwrap()
}

#[test]
fn multiple_versions_remain_distinct() {
    let (workspace, store) = setup();
    let ws = workspace.path();
    let model = import_model(ws, &store);

    // 1. Distinct package identities: two `alpha` packages with different
    // versions, same registry source.
    let alphas: Vec<&tong_rust::Package> = model
        .packages
        .iter()
        .filter(|pkg| pkg.name == "alpha")
        .collect();
    assert_eq!(alphas.len(), 2, "both alpha versions must be imported");
    let ids: Vec<&PackageId> = alphas.iter().map(|pkg| &pkg.id).collect();
    assert_ne!(ids[0], ids[1]);
    assert_eq!(ids[0].name, "alpha");
    assert_eq!(ids[1].name, "alpha");
    assert!(ids.iter().any(|id| id.version.major == 1));
    assert!(ids.iter().any(|id| id.version.major == 2));
    // The app's edges point at the exact identities.
    let app = model.packages.iter().find(|pkg| pkg.name == "app").unwrap();
    assert_eq!(app.deps.len(), 2);
    let dep_ids: Vec<&PackageId> = app.deps.iter().map(|dep| &dep.package).collect();
    assert_eq!(dep_ids, vec![ids[0], ids[1]]);

    // 2. Feature sets resolve separately per identity.
    let requests: Vec<FeatureRequest> = model
        .members
        .iter()
        .map(|id| FeatureRequest {
            package: id.clone(),
            features: Vec::new(),
            default_features: true,
        })
        .collect();
    let map = resolve_features(&model, &requests, false).unwrap();
    for id in &ids {
        assert!(map.packages.contains_key(id));
    }

    // 3. Lock edges name both versions exactly.
    let lock = tong_fetch::TongLock::load(ws).unwrap();
    let locked_alphas: Vec<&tong_fetch::LockedPackage> = lock.candidates("alpha").collect();
    assert_eq!(locked_alphas.len(), 2);
    let versions: Vec<String> = locked_alphas
        .iter()
        .map(|pkg| pkg.version.to_string())
        .collect();
    assert_eq!(versions, vec!["1.0.0".to_owned(), "2.0.0".to_owned()]);
    let app_locked = lock.candidates("app").next().expect("app is locked");
    assert_eq!(app_locked.dependencies.len(), 2);

    // 4. Action ids and digests stay distinct and deterministic.
    let cas = Cas::open(&store).unwrap();
    let toolchain = capture_system_rust(&cas).expect("capture system rustc");
    let mut backend = tong_rust::RustBackend::new(cas.clone(), &model, toolchain, "dev").unwrap();
    let planned = backend.plan().unwrap();
    let lib_ids: Vec<&str> = planned
        .iter()
        .map(|action| action.logical_id.0.as_str())
        .filter(|id| id.starts_with("rust:lib:alpha"))
        .collect();
    assert_eq!(lib_ids.len(), 2, "two lib actions: {lib_ids:?}");
    // The disambiguated labels must differ.
    assert_ne!(lib_ids[0], lib_ids[1]);
    // Both are planned; their specs must produce distinct digests (no
    // version aliasing in the cache key).
    struct NoCompleted;
    impl tong_graph::Completed for NoCompleted {
        fn output_tree(
            &self,
            _: &tong_core::action::ActionId,
        ) -> Option<tong_core::artifact::TreeDigest> {
            None
        }
        fn stdout(
            &self,
            _: &tong_core::action::ActionId,
        ) -> Option<tong_core::artifact::BlobDigest> {
            None
        }
        fn stderr(
            &self,
            _: &tong_core::action::ActionId,
        ) -> Option<tong_core::artifact::BlobDigest> {
            None
        }
    }
    let concretize = |action: &tong_graph::PlannedAction| -> tong_core::digest::Digest {
        (action.make)(&NoCompleted, &cas)
            .expect("concretize")
            .digest()
    };
    let digests: Vec<tong_core::digest::Digest> = lib_ids
        .iter()
        .map(|id| {
            planned
                .iter()
                .find(|action| action.logical_id.0 == *id)
                .expect("planned action")
        })
        .map(concretize)
        .collect();
    assert_ne!(digests[0], digests[1]);

    // Determinism: an identical second plan produces identical digests
    // (identical action outputs deduplicate by digest). Actions with
    // dependencies concretize against a deterministic per-action-id stub
    // tree, so both plans see identical dependency outputs.
    struct StubCompleted(BTreeMap<tong_core::action::ActionId, tong_core::artifact::TreeDigest>);
    impl tong_graph::Completed for StubCompleted {
        fn output_tree(
            &self,
            id: &tong_core::action::ActionId,
        ) -> Option<tong_core::artifact::TreeDigest> {
            self.0.get(id).cloned()
        }
        fn stdout(
            &self,
            _: &tong_core::action::ActionId,
        ) -> Option<tong_core::artifact::BlobDigest> {
            None
        }
        fn stderr(
            &self,
            _: &tong_core::action::ActionId,
        ) -> Option<tong_core::artifact::BlobDigest> {
            None
        }
    }
    let stub = StubCompleted(
        planned
            .iter()
            .map(|action| {
                let tree = cas
                    .put_tree(&tong_core::tree::Tree::default())
                    .expect("placeholder tree");
                (action.logical_id.clone(), tree)
            })
            .collect(),
    );
    let concretize_stubbed = |action: &tong_graph::PlannedAction| -> tong_core::digest::Digest {
        (action.make)(&stub, &cas).expect("concretize").digest()
    };
    let cas2 = Cas::open(&store).unwrap();
    let toolchain2 = capture_system_rust(&cas2).expect("capture system rustc");
    let mut backend2 = tong_rust::RustBackend::new(cas2, &model, toolchain2, "dev").unwrap();
    let planned2 = backend2.plan().unwrap();
    for action in &planned {
        let again = planned2
            .iter()
            .find(|other| other.logical_id == action.logical_id)
            .expect("same logical id replanned");
        assert_eq!(
            concretize_stubbed(action),
            concretize_stubbed(again),
            "deterministic digests for {}",
            action.logical_id.0
        );
    }
}
