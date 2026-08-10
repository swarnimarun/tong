//! Local-registry integration tests: a hermetic `file://` registry fixture
//! with index files and gzipped `.crate` archives whose checksums match the
//! index (no real network).

use std::fs;
use std::path::{Path, PathBuf};

use tong_core::digest::{Digest, Hasher};
use tong_fetch::{IndexClient, LocalPackage, RegistryConfig, ResolvedDep, TongLock, resolve};
use tong_store::Cas;

/// A crate published to the fixture registry.
struct FixtureCrate {
    name: &'static str,
    version: &'static str,
    deps: Vec<(&'static str, &'static str, bool)>,
    lib: &'static str,
    yanked: bool,
}

/// Builds a gzipped tar `.crate` and returns (checksum, file name).
fn make_crate_archive(dir: &Path, fixture: &FixtureCrate) -> (String, String) {
    let name = fixture.name;
    let version = fixture.version;
    let top = format!("{name}-{version}");
    let build = dir.join("build").join(&top);
    fs::create_dir_all(build.join("src")).unwrap();
    fs::write(
        build.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    fs::write(build.join("src/lib.rs"), fixture.lib).unwrap();

    let archive_path = dir.join("crates").join(name).join(format!("{top}.crate"));
    fs::create_dir_all(archive_path.parent().unwrap()).unwrap();
    let file = fs::File::create(&archive_path).unwrap();
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    tar.append_dir_all(&top, &build).unwrap();
    let gz = tar.into_inner().unwrap();
    gz.finish().unwrap();

    let bytes = fs::read(&archive_path).unwrap();
    let checksum = Hasher::digest(&bytes).to_hex();
    (
        checksum,
        archive_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned(),
    )
}

/// Writes one sparse-index line.
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
    let deps = format!("[{}]", deps.join(","));
    let line = format!(
        "{{\"name\":\"{}\",\"vers\":\"{}\",\"deps\":{},\"cksum\":\"{}\",\"features\":{{}},\
         \"yanked\":{},\"v\":1}}\n",
        fixture.name, fixture.version, deps, checksum, fixture.yanked
    );
    let mut existing = fs::read_to_string(&full).unwrap_or_default();
    existing.push_str(&line);
    fs::write(&full, existing).unwrap();
}

/// The fixture registry: config.json + index + crates.
struct FixtureRegistry {
    root: PathBuf,
    /// The registry config (file:// index).
    config: RegistryConfig,
}

fn build_fixture() -> FixtureRegistry {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::mem::forget(dir); // keep alive for the test's duration

    let crates = vec![
        FixtureCrate {
            name: "alpha",
            version: "1.0.0",
            deps: Vec::new(),
            lib: "pub fn alpha() -> u32 { 7 }\n",
            yanked: false,
        },
        FixtureCrate {
            name: "alpha",
            version: "2.0.0",
            deps: Vec::new(),
            lib: "pub fn alpha() -> u32 { 70 }\n",
            yanked: false,
        },
        FixtureCrate {
            name: "beta",
            version: "1.0.0",
            deps: vec![("alpha", "^1", false)],
            lib: "pub fn beta() -> u32 { alpha::alpha() + 1 }\n",
            yanked: false,
        },
        FixtureCrate {
            name: "gamma",
            version: "1.0.0",
            deps: Vec::new(),
            lib: "pub fn gamma() -> u32 { 3 }\n",
            yanked: true,
        },
    ];
    for fixture in &crates {
        let (checksum, _) = make_crate_archive(&root, fixture);
        write_index_entry(&root, fixture, &checksum);
    }
    fs::write(
        root.join("index").join("config.json"),
        format!(
            "{{\"dl\":\"file://{}/crates/{{crate}}/{{crate}}-{{version}}.crate\",\"api\":null}}\n",
            root.display()
        ),
    )
    .unwrap();
    let config = RegistryConfig::from_url(&format!("file://{}/index", root.display())).unwrap();
    FixtureRegistry { root, config }
}

/// The index client for the fixture.
fn client(registry: &FixtureRegistry) -> IndexClient {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("index");
    std::mem::forget(dir);
    IndexClient::new(cache, registry.config.clone())
}

fn edge(name: &str, req: &str) -> ResolvedDep {
    ResolvedDep {
        name: name.to_owned(),
        req: Some(semver::VersionReq::parse(req).unwrap()),
        optional: false,
        dev: false,
        features: Vec::new(),
        default_features: true,
    }
}

/// A synthetic local root carrying the given registry edges.
fn root(deps: Vec<ResolvedDep>) -> LocalPackage {
    LocalPackage {
        name: "root".to_owned(),
        version: semver::Version::new(0, 1, 0),
        source: Some("path+.".to_owned()),
        deps,
    }
}

#[test]
fn versions_parses_the_fixture_index() {
    let registry = build_fixture();
    let client = client(&registry);
    let versions = client.versions("alpha").unwrap();
    assert_eq!(versions.len(), 2);
    assert!(!versions.iter().any(|v| v.yanked));
    let gamma = client.versions("gamma").unwrap();
    assert_eq!(gamma.len(), 1);
    assert!(gamma[0].yanked);
    let beta = client.versions("beta").unwrap();
    assert_eq!(beta[0].deps[0].name, "alpha");
    assert_eq!(
        beta[0].deps[0].req,
        semver::VersionReq::parse("^1").unwrap()
    );
}

#[test]
fn resolve_picks_highest_and_backtracks() {
    let registry = build_fixture();
    let client = client(&registry);
    let locked = TongLock::default();
    // beta 1.0.0 requires alpha ^1; roots beta * + alpha * pick alpha 2.0.0
    // initially, then backtrack to alpha 1.0.0 (alpha 2.0.0 is compatible
    // with beta's ^1 only if... ^1 excludes 2.0.0, so the resolver must
    // choose alpha 1.0.0).
    let packages = resolve(
        &client,
        &[root(vec![edge("alpha", "*"), edge("beta", "*")])],
        &locked,
    )
    .unwrap();
    let alpha = packages.iter().find(|p| p.name == "alpha").unwrap();
    let beta = packages.iter().find(|p| p.name == "beta").unwrap();
    assert_eq!(alpha.version.to_string(), "1.0.0");
    assert_eq!(beta.version.to_string(), "1.0.0");
    // Yanked gamma is never selected.
    assert!(packages.iter().all(|p| p.name != "gamma"));
}

#[test]
fn resolve_prefers_locked_version() {
    let registry = build_fixture();
    let client = client(&registry);
    let mut locked = TongLock::default();
    locked.packages.push(tong_fetch::LockedPackage {
        name: "alpha".to_owned(),
        version: semver::Version::new(1, 0, 0),
        source: "registry+file://fixture".to_owned(),
        checksum: Some("x".to_owned()),
        manifest_checksum: None,
        yanked: false,
        publish_time: None,
        dependencies: Vec::new(),
    });
    let packages = resolve(&client, &[root(vec![edge("alpha", "*")])], &locked).unwrap();
    let alpha = packages.iter().find(|p| p.name == "alpha").unwrap();
    assert_eq!(alpha.version.to_string(), "1.0.0");
}

#[test]
fn fetch_crate_verifies_and_rejects_corruption() {
    let registry = build_fixture();
    let store = tempfile::tempdir().unwrap();
    let cas = Cas::open(store.path().join("store")).unwrap();
    let client = client(&registry);
    let packages = resolve(
        &client,
        &[root(vec![edge("alpha", "*")])],
        &TongLock::default(),
    )
    .unwrap();
    let alpha = packages.iter().find(|p| p.name == "alpha").unwrap();
    let tree = tong_fetch::fetch_crate(&cas, &registry.config, alpha).unwrap();
    assert_eq!(
        tree,
        tong_fetch::fetch_crate(&cas, &registry.config, alpha).unwrap()
    );

    // Corrupt the stored blob: fetch must now reject it with Checksum.
    let blob = tong_fetch::crate_blob_path(cas.root(), alpha.checksum.as_deref().unwrap());
    let mut bytes = fs::read(&blob).unwrap();
    bytes[0] ^= 0xff;
    fs::write(&blob, &bytes).unwrap();
    let err = tong_fetch::fetch_crate(&cas, &registry.config, alpha).unwrap_err();
    assert!(
        matches!(err, tong_fetch::FetchError::Checksum { .. }),
        "{err}"
    );
}

/// A whole-checkout helper for the driver-level e2e test in `tong`.
pub fn fixture_registry() -> (PathBuf, RegistryConfig) {
    let registry = build_fixture();
    (registry.root, registry.config)
}

/// The digest helper used by the fixture (exported for `tong`'s e2e test).
pub fn sha256(bytes: &[u8]) -> Digest {
    Hasher::digest(bytes)
}
