//! The dynamic external-project corpus runner.
//!
//! Fetches pinned upstream repositories (Tokio, wgpu, …) once, then
//! differentially resolves and builds them with Tong against Cargo as an
//! offline oracle. Only `fetch` touches the network; `resolve` and `build`
//! reuse the per-entry homes and stores. The corpus configuration lives in
//! the checked-in `tests/corpus.toml` at the repository root; results are
//! written to `$TONG_CORPUS_DIR/report.json` (never rewriting the config).
//!
//! Checkout transport reuses `tong_fetch::git` (gix) — there is no second
//! Git implementation and no ambient `git` binary here.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tong_store::Cas;

/// The repository root (workspace root = two levels above this crate).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tools/")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

/// The corpus checkout root; `$TONG_CORPUS_DIR` or `target/tong-corpus`.
fn corpus_dir() -> PathBuf {
    std::env::var_os("TONG_CORPUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target").join("tong-corpus"))
}

/// The built tong binary: `$TONG_BIN` or `target/debug/tong`.
fn tong_bin() -> PathBuf {
    std::env::var_os("TONG_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target").join("debug").join("tong"))
}

/// The pinned Rust toolchain for every corpus subprocess.
const RUST_TOOLCHAIN: &str = "1.97.1";

// ---------------------------------------------------------------------------
// Corpus configuration (`tests/corpus.toml`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
struct CorpusConfig {
    /// Pinned toolchain version applied to every subprocess.
    #[serde(default = "default_toolchain")]
    rust_toolchain: String,
    entries: BTreeMap<String, CorpusEntry>,
}

impl CorpusConfig {
    /// The effective toolchain: the config's pin.
    fn toolchain(&self) -> &str {
        &self.rust_toolchain
    }
}

fn default_toolchain() -> String {
    RUST_TOOLCHAIN.to_owned()
}

#[derive(Clone, Debug, Deserialize)]
struct CorpusEntry {
    /// Clone URL (HTTPS).
    url: String,
    /// Exact commit to check out and verify.
    rev: String,
    /// `required` entries must pass every gate; `extended` entries record
    /// observed evidence and never block.
    tier: Tier,
    /// The Cargo oracle invocation (run with `--offline` after fetch).
    #[serde(default = "default_cargo")]
    cargo: Vec<String>,
    /// The Tong command executed by `build` (`--offline` is appended).
    #[serde(default = "default_tong")]
    tong: Vec<String>,
    /// Gate semantics: default (resolver equality + `tong`), tokio
    /// (full-workspace metadata equality + test --no-run), or wgpu
    /// (full-workspace resolver equality + five-package all-features
    /// check). The `tong` field is replaced by the override's command.
    #[serde(default = "default_gate")]
    gate: Gate,
    /// Extra host binaries the entry needs beyond cargo/rustc.
    #[serde(default)]
    tools: Vec<String>,
    /// Host OS expectations: `linux`/`macos`/`windows`/`all`. Entries
    /// whose list excludes the current host are marked `skip` with the
    /// declared reason instead of failing.
    #[serde(default = "all_platforms")]
    platforms: Vec<String>,
    #[serde(default)]
    skip_reason: String,
    /// Documented divergence for entries that do not yet pass the required
    /// gate (see docs/corpus-divergences.md); empty for expected `pass`.
    #[serde(default)]
    divergence: String,
}

fn default_cargo() -> Vec<String> {
    vec!["metadata".into(), "--format-version".into(), "1".into()]
}
fn default_tong() -> Vec<String> {
    vec!["check".into(), "--workspace".into(), "--all-targets".into()]
}
fn default_gate() -> Gate {
    Gate::Default
}
fn all_platforms() -> Vec<String> {
    vec!["all".into()]
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Tier {
    Required,
    Extended,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Gate {
    Default,
    Tokio,
    Wgpu,
}

impl CorpusEntry {
    fn skip_on_host(&self) -> Option<String> {
        let host = std::env::consts::OS;
        let want = match host {
            "macos" => "macos",
            "windows" => "windows",
            _ => "linux",
        };
        if self.platforms.iter().any(|p| p == "all" || p == want) {
            None
        } else if self.skip_reason.is_empty() {
            Some(format!("platform {want} is not in the declared platforms"))
        } else {
            Some(self.skip_reason.clone())
        }
    }
}

fn load_config() -> CorpusConfig {
    let path = workspace_root().join("tests").join("corpus.toml");
    let text = fs::read_to_string(&path).expect("tests/corpus.toml");
    toml::from_str(&text).expect("tests/corpus.toml is valid TOML")
}

fn selected_entries(
    config: &CorpusConfig,
    tier: &str,
    only: Option<&str>,
) -> Vec<(String, CorpusEntry)> {
    config
        .entries
        .iter()
        .filter(|(name, _)| only.is_none_or(|only| name.as_str() == only))
        .filter(|(_, entry)| match tier {
            "required" => entry.tier == Tier::Required,
            "extended" => entry.tier == Tier::Extended,
            _ => true,
        })
        .map(|(name, entry)| (name.clone(), entry.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Entry directories
// ---------------------------------------------------------------------------

struct EntryDirs {
    root: PathBuf,
    src: PathBuf,
    cargo_home: PathBuf,
    cargo_target: PathBuf,
    store: PathBuf,
}

fn entry_dirs(name: &str) -> EntryDirs {
    let root = corpus_dir().join(name);
    EntryDirs {
        src: root.join("src"),
        cargo_home: root.join("cargo-home"),
        cargo_target: root.join("cargo-target"),
        store: root.join("tong-store"),
        root,
    }
}

/// The environment every corpus subprocess gets: pinned toolchain, the
/// per-entry cargo home/target, and the per-entry Tong store.
/// The base environment for every corpus subprocess (pinned toolchain,
/// per-entry cargo home/target, per-entry Tong store). `fetch` is the
/// networked phase and omits `CARGO_NET_OFFLINE`; `resolve`/`build` add
/// it so the oracle and Tong never touch the network.
fn subprocess_env(config: &CorpusConfig, dirs: &EntryDirs, offline: bool) -> Vec<(String, String)> {
    let mut env = vec![
        ("RUSTUP_TOOLCHAIN".to_owned(), config.toolchain().to_owned()),
        (
            "CARGO_HOME".to_owned(),
            dirs.cargo_home.display().to_string(),
        ),
        (
            "CARGO_TARGET_DIR".to_owned(),
            dirs.cargo_target.display().to_string(),
        ),
        (
            "TONG_STORE_DIR".to_owned(),
            dirs.store.display().to_string(),
        ),
        (
            "TONG_CACHE_DIR".to_owned(),
            dirs.store.display().to_string(),
        ),
    ];
    if offline {
        env.push(("CARGO_NET_OFFLINE".to_owned(), "true".to_owned()));
    }
    env
}

fn run_in(
    dir: &Path,
    program: &Path,
    args: &[String],
    env: &[(String, String)],
) -> std::process::Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .env("TMPDIR", std::env::var_os("TMPDIR").unwrap_or_default())
        .envs(env.iter().cloned())
        .output()
        .expect("spawn subprocess")
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn refresh_tong_lock(dirs: &EntryDirs, env: &[(String, String)]) -> Result<(), String> {
    let output = run_in(
        &dirs.src,
        &tong_bin(),
        &["lock".to_owned(), "--offline".to_owned()],
        env,
    );
    if output.status.success() {
        Ok(())
    } else if dirs.src.join("Tong.lock").is_file() {
        eprintln!(
            "  lock refresh unavailable; using existing Tong.lock: {}",
            stderr_of(&output).trim()
        );
        Ok(())
    } else {
        Err(format!(
            "tong lock --offline failed: {}",
            stderr_of(&output)
        ))
    }
}

/// Imports Cargo's explicitly configured git checkout cache into Tong's
/// CAS before the corpus lock step. This is required when an upstream
/// commit is pinned in Cargo.lock but is no longer advertised by the
/// remote; the corpus still has the exact source fetched by Cargo.
fn import_cargo_git_checkouts(dirs: &EntryDirs) -> Result<(), String> {
    #[derive(Deserialize)]
    struct CargoLock {
        #[serde(default)]
        package: Vec<CargoLockPackage>,
    }
    #[derive(Deserialize)]
    struct CargoLockPackage {
        name: String,
        version: semver::Version,
        source: Option<String>,
    }
    #[derive(Deserialize)]
    struct CargoManifest {
        package: Option<CargoManifestPackage>,
    }
    #[derive(Deserialize)]
    struct CargoManifestPackage {
        name: String,
        version: String,
    }

    let cargo_lock: CargoLock = toml::from_str(
        &fs::read_to_string(dirs.src.join("Cargo.lock")).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())?;
    let git_packages: BTreeMap<(String, semver::Version), String> = cargo_lock
        .package
        .into_iter()
        .filter_map(|package| {
            package
                .source
                .filter(|source| source.starts_with("git+"))
                .map(|source| ((package.name, package.version), source))
        })
        .collect();
    if git_packages.is_empty() {
        return Ok(());
    }

    let cas = Cas::open(&dirs.store).map_err(|err| err.to_string())?;
    let mut lock = tong_fetch::TongLock::load(&dirs.src).unwrap_or_default();
    let checkouts = dirs.cargo_home.join("git/checkouts");
    let repositories = fs::read_dir(&checkouts).map_err(|err| err.to_string())?;
    for repository in repositories.flatten() {
        let revisions = match fs::read_dir(repository.path()) {
            Ok(revisions) => revisions,
            Err(_) => continue,
        };
        for revision in revisions.flatten() {
            let checkout = revision.path();
            let manifest_path = checkout.join("Cargo.toml");
            let Ok(text) = fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(manifest) = toml::from_str::<CargoManifest>(&text) else {
                continue;
            };
            let Some(package) = manifest.package else {
                continue;
            };
            let Ok(version) = semver::Version::parse(&package.version) else {
                continue;
            };
            let Some(cargo_source) = git_packages.get(&(package.name.clone(), version.clone()))
            else {
                continue;
            };
            let Some((location, commit)) = cargo_source
                .strip_prefix("git+")
                .and_then(|source| source.rsplit_once('#'))
            else {
                continue;
            };
            let url = location
                .split('?')
                .next()
                .unwrap_or(location)
                .trim_end_matches(".git");
            let source = format!("git+{url}#{commit}");
            let tree = cas
                .capture_dir_filtered(&checkout, &[".git", "target"].into_iter().collect())
                .map_err(|err| err.to_string())?;
            let manifest_checksum = tong_core::digest::Hasher::digest(text.as_bytes()).to_hex();
            lock.packages.retain(|locked| {
                !(locked.name == package.name && locked.source.starts_with(&format!("git+{url}#")))
            });
            lock.packages.push(tong_fetch::LockedPackage {
                name: package.name,
                version,
                source,
                checksum: None,
                manifest_checksum: Some(manifest_checksum),
                tree_digest: Some(tree.digest().to_hex()),
                yanked: false,
                publish_time: None,
                dependencies: Vec::new(),
            });
        }
    }
    lock.save(&dirs.src).map_err(|err| err.to_string())
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Report {
    schema: u32,
    generated: String,
    platform: String,
    entries: BTreeMap<String, EntryReport>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct EntryReport {
    tier: String,
    rev: String,
    #[serde(default)]
    resolver: Option<GateResult>,
    #[serde(default)]
    build: Option<GateResult>,
    #[serde(default)]
    skip: Option<String>,
    #[serde(default)]
    divergence: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct GateResult {
    status: String, // pass | fail | skip
    detail: String,
    duration_ms: u64,
}

fn load_report() -> Report {
    let path = corpus_dir().join("report.json");
    if let Ok(text) = fs::read_to_string(&path)
        && let Ok(report) = serde_json::from_str(&text)
    {
        report
    } else {
        Report {
            schema: 1,
            generated: String::new(),
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            entries: BTreeMap::new(),
        }
    }
}

fn save_report(report: &Report) {
    fs::create_dir_all(corpus_dir()).unwrap();
    let path = corpus_dir().join("report.json");
    let json = serde_json::to_string_pretty(report).unwrap();
    fs::write(path, json).unwrap();
}

fn refresh_report_metadata(report: &mut Report, config: &CorpusConfig) {
    report.schema = 1;
    report.generated = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    report.platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    for (name, entry) in &config.entries {
        let row = report.entries.entry(name.clone()).or_default();
        row.tier = match entry.tier {
            Tier::Required => "required",
            Tier::Extended => "extended",
        }
        .to_owned();
        row.rev = entry.rev.clone();
        row.divergence = (!entry.divergence.is_empty()).then(|| entry.divergence.clone());
    }
}

// ---------------------------------------------------------------------------
// fetch: clone, verify, populate homes (the only networked phase)
// ---------------------------------------------------------------------------

fn cmd_fetch(tier: &str, only: Option<&str>) {
    let config = load_config();
    for (name, entry) in selected_entries(&config, tier, only) {
        println!("fetching {name} @ {}", entry.rev);
        if let Some(reason) = entry.skip_on_host() {
            println!("  skip: {reason}");
            continue;
        }
        let dirs = entry_dirs(&name);
        fs::create_dir_all(&dirs.root).unwrap();
        // Missing host tools fail loudly: the entry cannot be validated.
        for tool in &entry.tools {
            let probe = Command::new("which").arg(tool).output().expect("which");
            assert!(probe.status.success(), "{name} requires host tool {tool:?}");
        }
        let cas = Cas::open(&dirs.store).unwrap();
        let resolved = tong_fetch::git::resolve_and_capture(
            &dirs.store,
            &cas,
            &entry.url,
            Some(&entry.rev),
            None,
            None,
            false,
            None,
        )
        .unwrap_or_else(|err| panic!("{name}: git resolve failed: {err}"));
        assert_eq!(
            resolved.commit, entry.rev,
            "{name}: checkout commit {} != pinned {}",
            resolved.commit, entry.rev
        );
        // Materialize the verified tree into the corpus src dir. The
        // transient git checkout is dropped once the tree is captured
        // (slice 3: importers never retain extracted checkouts).
        if !dirs.src.is_dir() {
            fs::create_dir_all(&dirs.root).unwrap();
            fs::create_dir(&dirs.src).unwrap();
            cas.materialize(resolved.tree_digest, &dirs.src)
                .expect("materialize src tree");
            let _ = fs::remove_dir_all(&resolved.checkout);
        }
        let env = subprocess_env(&config, &dirs, false);
        // Cargo's index + crate cache (populates cargo-home so the oracle
        // can run --offline later). `--locked` needs a lockfile; upstream
        // entries pin commits that carry one, and lock-free fixtures
        // (local smoke repos) generate it offline first.
        if !dirs.src.join("Cargo.lock").is_file() {
            let gen_lock = run_in(
                &dirs.src,
                Path::new("cargo"),
                &["generate-lockfile"].map(String::from),
                &env,
            );
            assert!(
                gen_lock.status.success(),
                "{name}: no Cargo.lock and offline generation failed: {}",
                stderr_of(&gen_lock)
            );
        }
        // Import an already-present pinned git checkout before Cargo's
        // network/cache refresh too; this keeps the Tong path usable when
        // the remote no longer advertises the locked commit.
        import_cargo_git_checkouts(&dirs)
            .unwrap_or_else(|err| panic!("{name}: cannot import Cargo git cache: {err}"));
        let fetch = run_in(
            &dirs.src,
            Path::new("cargo"),
            &["fetch", "--locked"].map(String::from),
            &env,
        );
        assert!(
            fetch.status.success(),
            "{name}: cargo fetch failed: {}",
            stderr_of(&fetch)
        );
        // Tong lock + fetch against the same checkout.
        for sub in ["lock", "fetch"] {
            let tong = run_in(&dirs.src, &tong_bin(), &[sub.to_owned()], &env);
            assert!(
                tong.status.success(),
                "{name}: tong {sub} failed: {}",
                stderr_of(&tong)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// resolve: differential resolver equality against cargo metadata
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct View {
    /// Sorted `(name@version#source, resolved features)`.
    packages: Vec<(String, Vec<String>)>,
    /// Sorted `(from, to)` package identity edges.
    edges: Vec<(String, String)>,
    /// Sorted `(package id, target kind, target name)` rows.
    targets: Vec<(String, String, String)>,
}

/// Normalizes a cargo `source` string and a tong `lock_source` string to a
/// comparable identity: registry and git sources are verbatim; path and
/// workspace sources are both `path` (cargo reports `null`).
fn normalize_source(source: &str) -> String {
    if source.is_empty() {
        return "path".to_owned();
    }
    if source.starts_with("path+") {
        return "path".to_owned();
    }
    if let Some(git) = source.strip_prefix("git+") {
        let (location, commit) = git.split_once('#').unwrap_or((git, ""));
        // Cargo includes the requested branch/tag/rev in metadata ids,
        // while Tong's locked identity is the canonical repository plus
        // resolved commit. Once locked, selectors and a trailing `.git`
        // do not change package identity.
        let repository = location.split_once('?').map_or(location, |(url, _)| url);
        let repository = repository.strip_suffix(".git").unwrap_or(repository);
        return format!("git+{repository}#{commit}");
    }
    // crates.io has two index endpoints (the github index cargo prints and
    // the sparse index Tong uses); they are one registry.
    source.replace(
        "registry+https://github.com/rust-lang/crates.io-index",
        "registry+https://index.crates.io",
    )
}

/// Builds the tong-side view from `tong graph --format json`.
fn tong_view(graph: &Value) -> View {
    let mut view = View::default();
    for package in graph["packages"].as_array().unwrap_or(&Vec::new()) {
        let name = package["name"].as_str().unwrap_or("").to_owned();
        let version = package["version"].as_str().unwrap_or("").to_owned();
        let source = normalize_source(package["source"].as_str().unwrap_or(""));
        let id = format!("{name}@{version}#{source}");
        let mut features: Vec<String> = package["features"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str())
                    .map(|f| f.to_owned())
                    .collect()
            })
            .unwrap_or_default();
        features.sort();
        features.dedup();
        view.packages.push((id.clone(), features));
        let empty = Vec::new();
        let edges = package["edges"].as_array().unwrap_or(&empty);
        for edge in edges {
            let dep = edge["package"].as_str().unwrap_or("").to_owned();
            let dep_source = normalize_source(edge["source"].as_str().unwrap_or(""));
            view.edges.push((id.clone(), format!("{dep}#{dep_source}")));
        }
        let targets = package["targets"].as_array().unwrap_or(&empty);
        for target in targets {
            let kind = target["kind"].as_str().unwrap_or("").to_owned();
            let tname = target["name"].as_str().unwrap_or("").to_owned();
            view.targets.push((id.clone(), kind, tname));
        }
    }
    view.packages.sort();
    view.packages.dedup();
    view.edges.sort();
    view.edges.dedup();
    view.targets.sort();
    view.targets.dedup();
    view
}

/// Builds the cargo-side view from `cargo metadata --locked --offline`.
fn cargo_view(metadata: &Value) -> View {
    let mut view = View::default();
    let packages = metadata["packages"].as_array().unwrap();
    let mut id_to_key: BTreeMap<String, String> = BTreeMap::new();
    type PackageRows = (String, Vec<String>, Vec<(String, String, String)>);
    let mut package_keys: BTreeMap<String, PackageRows> = BTreeMap::new();
    for package in packages {
        let name = package["name"].as_str().unwrap_or("").to_owned();
        let version = package["version"].as_str().unwrap_or("").to_owned();
        let source = normalize_source(package["source"].as_str().unwrap_or(""));
        let key = format!("{name}@{version}#{source}");
        id_to_key.insert(package["id"].as_str().unwrap_or("").to_owned(), key.clone());
        // Declared feature names, sorted.
        let mut declared: Vec<String> = package["features"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        declared.sort();
        // Target kinds (mirroring cargo's `targets[].kind`).
        let mut targets: Vec<(String, String, String)> = Vec::new();
        for target in package["targets"].as_array().unwrap() {
            let kind = target["kind"]
                .as_array()
                .and_then(|k| k.first())
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .to_owned();
            let tname = target["name"].as_str().unwrap_or("").to_owned();
            targets.push((key.clone(), kind, tname));
        }
        package_keys.insert(key.clone(), (name, declared, targets));
    }
    // Resolved per-node features + active edges from `resolve`.
    let mut features: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    for node in metadata["resolve"]["nodes"].as_array().unwrap() {
        let key = id_to_key
            .get(node["id"].as_str().unwrap_or(""))
            .cloned()
            .unwrap_or_default();
        let mut node_features: Vec<String> = node["features"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str())
                    .map(|f| f.to_owned())
                    .collect()
            })
            .unwrap_or_default();
        node_features.sort();
        node_features.dedup();
        features.insert(key.clone(), node_features);
        for dep in node["dependencies"].as_array().unwrap() {
            let dep_id = dep.as_str().unwrap_or("");
            if let Some(dep_key) = id_to_key.get(dep_id) {
                edges.push((key.clone(), dep_key.clone()));
            }
        }
    }
    // Only nodes that appear in the resolve graph count (dev deps of
    // members included); lockfile-only packages are not build edges.
    for (key, (_, declared, targets)) in &package_keys {
        let resolved = features
            .get(key)
            .cloned()
            .unwrap_or_else(|| declared.clone());
        view.packages.push((key.clone(), resolved));
        for row in targets {
            view.targets.push(row.clone());
        }
    }
    view.packages.sort();
    view.packages.dedup();
    view.edges = edges;
    view.edges.sort();
    view.edges.dedup();
    view.targets.sort();
    view.targets.dedup();
    view
}

/// Compares the two views; returns a human-readable diff summary or None.
fn compare_views(tong: &View, cargo: &View) -> Option<String> {
    let mut diffs: Vec<String> = Vec::new();
    let tong_pkgs: BTreeMap<_, _> = tong.packages.iter().cloned().collect();
    let cargo_pkgs: BTreeMap<_, _> = cargo.packages.iter().cloned().collect();
    for (key, features) in &tong_pkgs {
        match cargo_pkgs.get(key) {
            None => diffs.push(format!("tong-only package {key}")),
            Some(cargo_features) if cargo_features != features => diffs.push(format!(
                "feature set differs for {key}: tong={:?} cargo={:?}",
                features, cargo_features
            )),
            _ => {}
        }
    }
    for key in cargo_pkgs.keys() {
        if !tong_pkgs.contains_key(key) {
            diffs.push(format!("cargo-only package {key}"));
        }
    }
    let tong_edges: std::collections::BTreeSet<_> = tong.edges.iter().cloned().collect();
    let cargo_edges: std::collections::BTreeSet<_> = cargo.edges.iter().cloned().collect();
    for edge in &tong.edges {
        if !cargo_edges.contains(edge) {
            diffs.push(format!("tong-only edge {} -> {}", edge.0, edge.1));
        }
    }
    for edge in &cargo.edges {
        if !tong_edges.contains(edge) {
            diffs.push(format!("cargo-only edge {} -> {}", edge.0, edge.1));
        }
    }
    let tong_targets: std::collections::BTreeSet<_> = tong.targets.iter().cloned().collect();
    let cargo_targets: std::collections::BTreeSet<_> = cargo.targets.iter().cloned().collect();
    for row in &tong.targets {
        if !cargo_targets.contains(row) {
            diffs.push(format!("tong-only target {:?}", row));
        }
    }
    for row in &cargo.targets {
        if !tong_targets.contains(row) {
            diffs.push(format!("cargo-only target {:?}", row));
        }
    }
    if diffs.is_empty() {
        None
    } else {
        diffs.truncate(20);
        Some(diffs.join("\n  "))
    }
}

fn cmd_resolve(tier: &str, only: Option<&str>) {
    let config = load_config();
    let mut report = load_report();
    refresh_report_metadata(&mut report, &config);
    let mut required_failures = 0u32;
    for (name, entry) in selected_entries(&config, tier, only) {
        println!("resolving {name}");
        if let Some(reason) = entry.skip_on_host() {
            report.entries.entry(name.clone()).or_default().skip = Some(reason);
            continue;
        }
        let dirs = entry_dirs(&name);
        if !entry.divergence.is_empty() {
            report.entries.entry(name.clone()).or_default().divergence =
                Some(entry.divergence.clone());
        }
        assert!(
            dirs.src.is_dir(),
            "{name}: not fetched; run `corpus fetch` first"
        );
        let env = subprocess_env(&config, &dirs, true);
        let started = std::time::Instant::now();
        if let Err(detail) = refresh_tong_lock(&dirs, &env) {
            report.entries.entry(name.clone()).or_default().resolver = Some(GateResult {
                status: "fail".into(),
                detail,
                duration_ms: started.elapsed().as_millis() as u64,
            });
            if entry.tier == Tier::Required {
                required_failures += 1;
            }
            continue;
        }
        // Cargo oracle (offline; the fetch phase populated the index).
        let mut cargo_args: Vec<String> = entry.cargo.clone();
        if !cargo_args.iter().any(|a| a == "--offline") {
            cargo_args.push("--offline".into());
        }
        let cargo_out = run_in(&dirs.src, Path::new("cargo"), &cargo_args, &env);
        if !cargo_out.status.success() {
            report.entries.entry(name.clone()).or_default().resolver = Some(GateResult {
                status: "fail".into(),
                detail: format!("cargo metadata failed: {}", stderr_of(&cargo_out)),
                duration_ms: started.elapsed().as_millis() as u64,
            });
            if entry.tier == Tier::Required {
                required_failures += 1;
            }
            continue;
        }
        let cargo_metadata: Value =
            serde_json::from_str(&stdout_of(&cargo_out)).expect("cargo metadata JSON");
        // Tong graph (offline via the store).
        let tong_out = run_in(
            &dirs.src,
            &tong_bin(),
            &["graph", "--format", "json", "--workspace", "--offline"].map(String::from),
            &env,
        );
        if !tong_out.status.success() {
            report.entries.entry(name.clone()).or_default().resolver = Some(GateResult {
                status: "fail".into(),
                detail: format!("tong graph failed: {}", stderr_of(&tong_out)),
                duration_ms: started.elapsed().as_millis() as u64,
            });
            if entry.tier == Tier::Required {
                required_failures += 1;
            }
            continue;
        }
        let tong_graph: Value =
            serde_json::from_str(&stdout_of(&tong_out)).expect("tong graph JSON");
        let tong = tong_view(&tong_graph);
        let cargo = cargo_view(&cargo_metadata);
        let diff = compare_views(&tong, &cargo);
        report.entries.entry(name.clone()).or_default().resolver = Some(GateResult {
            status: if diff.is_none() {
                "pass".into()
            } else {
                "fail".into()
            },
            detail: diff.clone().unwrap_or_else(|| "resolver equality".into()),
            duration_ms: started.elapsed().as_millis() as u64,
        });
        match &diff {
            None => println!("  resolver: pass ({} packages)", tong.packages.len()),
            Some(d) => {
                println!("  resolver: FAIL\n  {d}");
                if entry.tier == Tier::Required {
                    required_failures += 1;
                }
            }
        }
    }
    save_report(&report);
    // Required rows must pass (extended entries only record evidence).
    if required_failures > 0 {
        eprintln!("corpus: {required_failures} required resolver row(s) failed");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// build: execute the configured Tong command offline
// ---------------------------------------------------------------------------

fn cmd_build(tier: &str, only: Option<&str>) {
    let config = load_config();
    let mut report = load_report();
    refresh_report_metadata(&mut report, &config);
    let mut required_failures = 0u32;
    for (name, entry) in selected_entries(&config, tier, only) {
        println!("building {name}");
        if let Some(reason) = entry.skip_on_host() {
            report.entries.entry(name.clone()).or_default().skip = Some(reason);
            continue;
        }
        let dirs = entry_dirs(&name);
        assert!(
            dirs.src.is_dir(),
            "{name}: not fetched; run `corpus fetch` first"
        );
        let env = subprocess_env(&config, &dirs, true);
        let started = std::time::Instant::now();
        if let Err(detail) = refresh_tong_lock(&dirs, &env) {
            report.entries.entry(name.clone()).or_default().build = Some(GateResult {
                status: "fail".into(),
                detail,
                duration_ms: started.elapsed().as_millis() as u64,
            });
            if entry.tier == Tier::Required {
                required_failures += 1;
            }
            continue;
        }
        let mut args = build_command(&entry);
        if !args.iter().any(|a| a == "--offline") {
            args.push("--offline".into());
        }
        let out = run_in(&dirs.src, &tong_bin(), &args, &env);
        let status: String = if out.status.success() {
            "pass".into()
        } else {
            "fail".into()
        };
        let detail = if out.status.success() {
            format!("exit 0; tong {}{}", args.join(" "), "")
        } else {
            format!("exit {:?}: {}", out.status.code(), stderr_of(&out))
        };
        println!("  build: {status} ({} ms)", started.elapsed().as_millis());
        if !out.status.success() {
            println!(
                "{}",
                stderr_of(&out)
                    .lines()
                    .take(12)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        report.entries.entry(name.clone()).or_default().build = Some(GateResult {
            status,
            detail,
            duration_ms: started.elapsed().as_millis() as u64,
        });
        if !out.status.success() && entry.tier == Tier::Required {
            required_failures += 1;
        }
    }
    save_report(&report);
    if required_failures > 0 {
        eprintln!("corpus: {required_failures} required build row(s) failed");
        std::process::exit(1);
    }
}

/// The Tong argv for the entry's gate: the `tokio` gate replaces the
/// configured command with the full-workspace no-run test. Tokio's
/// unstable all-features combination requires an explicit
/// `--cfg tokio_unstable`, so it is not a valid stable Cargo baseline.
/// The `wgpu` gate uses the five-package all-features check.
fn build_command(entry: &CorpusEntry) -> Vec<String> {
    match entry.gate {
        Gate::Default => entry.tong.clone(),
        Gate::Tokio => vec!["test".into(), "--workspace".into(), "--no-run".into()],
        Gate::Wgpu => vec![
            "check".into(),
            "-p".into(),
            "wgpu".into(),
            "-p".into(),
            "wgpu-core".into(),
            "-p".into(),
            "wgpu-hal".into(),
            "-p".into(),
            "wgpu-types".into(),
            "-p".into(),
            "naga".into(),
            "--all-features".into(),
        ],
    }
}

// ---------------------------------------------------------------------------
// report: print the current report
// ---------------------------------------------------------------------------

fn cmd_report() {
    let report = load_report();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

#[derive(Parser)]
#[command(
    name = "tong-corpus-runner",
    about = "Pinned external Rust corpus for Tong"
)]
struct Cli {
    #[command(subcommand)]
    command: Command_,
}

#[derive(Subcommand)]
enum Command_ {
    /// Clone entries, verify pinned commits, populate cargo/tong caches.
    Fetch {
        #[arg(long, default_value = "required")]
        tier: String,
        #[arg(long)]
        entry: Option<String>,
    },
    /// Differential resolver equality: cargo metadata vs tong graph.
    Resolve {
        #[arg(long, default_value = "required")]
        tier: String,
        #[arg(long)]
        entry: Option<String>,
    },
    /// Execute each entry's Tong gate command offline.
    Build {
        #[arg(long, default_value = "required")]
        tier: String,
        #[arg(long)]
        entry: Option<String>,
    },
    /// Print the accumulated report.
    Report,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command_::Fetch { tier, entry } => cmd_fetch(&tier, entry.as_deref()),
        Command_::Resolve { tier, entry } => cmd_resolve(&tier, entry.as_deref()),
        Command_::Build { tier, entry } => cmd_build(&tier, entry.as_deref()),
        Command_::Report => cmd_report(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_source_merges_path_forms() {
        assert_eq!(normalize_source(""), "path");
        assert_eq!(normalize_source("path+app"), "path");
        assert_eq!(
            normalize_source("registry+https://github.com/rust-lang/crates.io-index"),
            "registry+https://index.crates.io"
        );
        assert_eq!(
            normalize_source("git+https://github.com/tokio-rs/tokio.git?rev=abc#abc123"),
            "git+https://github.com/tokio-rs/tokio#abc123"
        );
    }

    #[test]
    fn older_reports_without_divergence_still_load() {
        // A report written before the `divergence` field existed must
        // deserialize (all optional fields default) instead of resetting
        // the accumulated report on the next run.
        let text = r#"{"schema":1,"generated":"","platform":"x","entries":{"entry":{"tier":"required","rev":"abc","resolver":{"status":"fail","detail":"x","duration_ms":1}}}}"#;
        let report: Report = serde_json::from_str(text).expect("old report loads");
        let entry = &report.entries["entry"];
        assert_eq!(entry.tier, "required");
        assert!(entry.divergence.is_none());
        assert!(entry.skip.is_none());
        assert!(entry.build.is_none());
    }

    #[test]
    fn report_metadata_tracks_config() {
        let config = CorpusConfig {
            rust_toolchain: default_toolchain(),
            entries: BTreeMap::from([(
                "fixture".to_owned(),
                CorpusEntry {
                    url: "https://example.invalid/repo".to_owned(),
                    rev: "abc123".to_owned(),
                    tier: Tier::Required,
                    cargo: default_cargo(),
                    tong: default_tong(),
                    gate: Gate::Default,
                    tools: Vec::new(),
                    platforms: all_platforms(),
                    skip_reason: String::new(),
                    divergence: String::new(),
                },
            )]),
        };
        let mut report = Report::default();
        refresh_report_metadata(&mut report, &config);
        assert_eq!(report.schema, 1);
        assert!(!report.generated.is_empty());
        assert!(!report.platform.is_empty());
        assert_eq!(report.entries["fixture"].tier, "required");
        assert_eq!(report.entries["fixture"].rev, "abc123");
        assert!(report.entries["fixture"].divergence.is_none());
    }

    #[test]
    fn views_compare_sorted() {
        let mut tong = View::default();
        tong.packages
            .push(("a@1.0.0#path".into(), vec!["default".into()]));
        tong.edges
            .push(("a@1.0.0#path".into(), "b@1.0.0#path".into()));
        let mut cargo = tong.clone();
        assert_eq!(compare_views(&tong, &cargo), None);
        cargo.packages[0].1.push("extra".into());
        cargo.packages[0].1.sort();
        assert!(compare_views(&tong, &cargo).is_some());
    }
}
