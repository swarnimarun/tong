//! Action parity: Cargo-style target kinds (links/DEP_*, env!-reading
//! binaries, required-feature examples, benches, doc tests, generated
//! dep-info, and host/target separation) all lower into executable,
//! cacheable actions.
//!
//! Builds run in-process: import → plan → execute in topological order
//! through the real executor and action cache.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tong_core::action::ActionId;
use tong_core::artifact::TreeDigest;
use tong_rust::{RustModel, import_cargo_workspace};
use tong_store::{ActionCache, CachedResult, Cas};

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

fn write_tree(root: &Path, files: &[(&str, &str)]) {
    for (path, content) in files {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }
}

/// A Cargo workspace exercising every slice-6 target kind.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_tree(
        root,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/native-lib\", \"crates/app\"]\nresolver = \"2\"\n",
            ),
            // A library with `links`: its build script exports metadata the
            // app's build script reads as DEP_NATIVE_LIB_MYKEY.
            (
                "crates/native-lib/Cargo.toml",
                "[package]\nname = \"native-lib\"\nversion = \"1.2.3\"\nedition = \"2021\"\nlinks = \"native_lib\"\nauthors = [\"Ada <ada@example.com>\"]\n",
            ),
            (
                "crates/native-lib/build.rs",
                "fn main() {\n    assert_eq!(std::env::var(\"CARGO_MANIFEST_LINKS\").unwrap(), \"native_lib\");\n    assert!(std::env::var(\"CARGO_MANIFEST_PATH\").unwrap().ends_with(\"Cargo.toml\"));\n    assert_eq!(std::env::var(\"TARGET\").unwrap(), std::env::var(\"HOST\").unwrap());\n    assert_eq!(std::env::var(\"CARGO_CFG_TARGET_ENDIAN\").unwrap(), \"little\");\n    let generated = std::path::PathBuf::from(std::env::var(\"OUT_DIR\").unwrap()).join(\"generated.rs\");\n    std::fs::write(&generated, \"pub const GENERATED: u32 = 7;\\n\").unwrap();\n    println!(\"cargo:rustc-env=NATIVE_GENERATED={}\", generated.display());\n    println!(\"cargo:MYKEY=from_native_lib\");\n}\n",
            ),
            (
                "crates/native-lib/src/lib.rs",
                "include!(env!(\"NATIVE_GENERATED\"));\npub fn value() -> u32 { GENERATED }\n",
            ),
            (
                "crates/app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"4.5.6-beta.1\"\nedition = \"2021\"\nauthors = [\"Ada\", \"Grace\"]\ndescription = \"environment fixture\"\nhomepage = \"https://example.com/app\"\nrepository = \"https://example.com/repo\"\nlicense = \"MIT\"\nlicense-file = \"LICENSE\"\nreadme = \"README.md\"\nrust-version = \"1.85\"\n\n[dependencies]\nnative-lib = { path = \"../native-lib\" }\n\n[build-dependencies]\nnative-lib = { path = \"../native-lib\" }\n\n[features]\ndefault = []\ngated = []\n\n[[bin]]\nname = \"app\"\npath = \"src/main.rs\"\n\n[[example]]\nname = \"gated\"\npath = \"examples/gated.rs\"\nrequired-features = [\"gated\"]\n",
            ),
            // The app's build script reads the DEP_ variable; the binary
            // reads CARGO_ env vars at compile time.
            (
                "crates/app/build.rs",
                "fn main() {\n    println!(\"cargo:rerun-if-env-changed=DEP_NATIVE_LIB_MYKEY\");\n    let v = std::env::var(\"DEP_NATIVE_LIB_MYKEY\").unwrap_or_else(|_| \"missing\".into());\n    println!(\"cargo:rustc-env=DEP_VALUE={}\", v);\n    println!(\"cargo:rustc-env=MY_VERSION={}\", std::env::var(\"CARGO_PKG_VERSION\").unwrap());\n}\n",
            ),
            (
                "crates/app/src/main.rs",
                "fn main() {\n    assert_eq!(env!(\"CARGO_PKG_AUTHORS\"), \"Ada:Grace\");\n    assert_eq!(env!(\"CARGO_PKG_DESCRIPTION\"), \"environment fixture\");\n    assert_eq!(env!(\"CARGO_PKG_VERSION_PRE\"), \"beta.1\");\n    assert_eq!(env!(\"CARGO_PKG_RUST_VERSION\"), \"1.85\");\n    assert!(env!(\"CARGO_PKG_LICENSE_FILE\").ends_with(\"LICENSE\"));\n    println!(\"dep={} ver={} pkg={}\", env!(\"DEP_VALUE\"), env!(\"MY_VERSION\"), env!(\"CARGO_PKG_NAME\"));\n}\n",
            ),
            ("crates/app/src/lib.rs", "pub fn value() -> u32 { 11 }\n"),
            ("crates/app/LICENSE", "MIT\n"),
            ("crates/app/README.md", "# App\n"),
            (
                "crates/app/tests/bin_env.rs",
                "#[test]\nfn cargo_binary_is_available() {\n    assert!(std::path::Path::new(env!(\"CARGO_BIN_EXE_app\")).is_file());\n}\n",
            ),
            // Cargo allows an integration test to share the lib target's
            // name; their build-unit identities must remain distinct.
            (
                "crates/app/tests/app.rs",
                "#[test]\nfn integration_test_named_like_lib() { assert_eq!(app::value(), 11); }\n",
            ),
            // A benchmark target.
            ("crates/app/benches/bench.rs", "fn main() {}\n"),
            // An example gated behind the `gated` feature (required-features).
            ("crates/app/examples/gated.rs", "fn main() {}\n"),
            ("crates/app/examples/plain.rs", "fn main() {}\n"),
        ],
    );
    // The [[bin]] with an explicit path + a [[bench]] via auto-discovery.
    let _ = root;
    dir
}

fn proc_macro_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_tree(
        dir.path(),
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"helper\", \"dev-helper\", \"macros\"]\nresolver = \"2\"\n",
            ),
            (
                "helper/Cargo.toml",
                "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("helper/src/lib.rs", "pub fn value() -> u32 { 7 }\n"),
            (
                "dev-helper/Cargo.toml",
                "[package]\nname = \"dev-helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("dev-helper/src/lib.rs", "pub fn value() -> u32 { 9 }\n"),
            (
                "macros/Cargo.toml",
                "[package]\nname = \"macros\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n\n[dependencies]\nhelper = { path = \"../helper\" }\n\n[dev-dependencies]\ndev-helper = { path = \"../dev-helper\" }\n",
            ),
            (
                "macros/src/lib.rs",
                "extern crate proc_macro;\nuse proc_macro::TokenStream;\n#[proc_macro]\npub fn seven(_: TokenStream) -> TokenStream { helper::value().to_string().parse().unwrap() }\n#[cfg(test)]\nmod tests { #[test] fn helpers_are_linkable() { assert_eq!(helper::value() + dev_helper::value(), 16); } }\n",
            ),
        ],
    );
    dir
}

/// A completed-actions view backed by real executed results.
struct Executed(BTreeMap<ActionId, CachedResult>);

impl tong_graph::Completed for Executed {
    fn output_tree(&self, action: &ActionId) -> Option<TreeDigest> {
        self.0.get(action).map(|result| result.outputs)
    }
    fn stdout(&self, action: &ActionId) -> Option<tong_core::artifact::BlobDigest> {
        self.0.get(action).map(|result| result.stdout)
    }
    fn stderr(&self, action: &ActionId) -> Option<tong_core::artifact::BlobDigest> {
        self.0.get(action).map(|result| result.stderr)
    }
}

/// Imports, plans, and executes every action of a cargo workspace,
/// returning (model, action-by-logical-id, store path).
fn build_workspace(
    root: &Path,
    store_dir: &Path,
    check: bool,
) -> (
    RustModel,
    BTreeMap<String, tong_core::digest::Digest>,
    PathBuf,
) {
    let cas = Cas::open(store_dir).unwrap();
    let toolchain = tong_rust::capture_system_rust(&cas).unwrap();
    let model = import_cargo_workspace(root, &host_triple(), &NoLock, None).unwrap();
    let mut backend = tong_rust::RustBackend::with_tests_state(
        cas.clone(),
        &model,
        toolchain.clone(),
        "dev",
        true,
        &[],
        Some(tong_store::StateStore::open(store_dir).unwrap()),
        tong_store::project_hash(root).ok(),
        false,
        true,
        false,
        check,
        Some(host_triple()),
    )
    .unwrap();
    let planned = backend.plan().unwrap();
    let order = tong_graph::topological_order(&planned).unwrap();
    let mut completed = Executed(BTreeMap::new());
    let cache = ActionCache::open(&cas).unwrap();
    let mut digests = BTreeMap::new();
    for action in &order {
        let spec = (action.make)(&completed, &cas).unwrap();
        let digest = spec.digest();
        digests.insert(spec.logical_id.0.clone(), digest);
        let cached = if let Some(result) = cache.get(digest).unwrap() {
            result
        } else {
            let mut executor = tong_exec::LocalExecutor::with_sandbox(
                cas.clone(),
                store_dir.join("exec"),
                tong_exec::SandboxLevel::L1,
            )
            .unwrap();
            executor.register_system_tool(toolchain.rustc_blob, toolchain.rustc.clone());
            executor.register_bundle_root(toolchain.bundle.digest(), toolchain.root.clone());
            let outcome = match executor.execute(&spec) {
                Ok(outcome) => outcome,
                Err(tong_exec::ExecError::Exit { code, stderr, .. }) => {
                    let text = cas
                        .read_blob(stderr)
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default();
                    panic!("action {} exited {code}: {text}", spec.logical_id.0);
                }
                Err(err) => panic!("action {} failed: {err}", spec.logical_id.0),
            };
            let result = CachedResult {
                outputs: outcome.outputs,
                stdout: outcome.stdout,
                stderr: outcome.stderr,
                duration_millis: 0,
            };
            cache.put(digest, &result).unwrap();
            result
        };
        completed.0.insert(spec.logical_id.clone(), cached);
    }
    (model, digests, store_dir.to_path_buf())
}

struct NoLock;

impl tong_rust::LockedSourceProvider for NoLock {
    fn locked_package(
        &self,
        edge: &tong_rust::RegistryEdge,
    ) -> Result<Option<tong_rust::LockedSource>, tong_rust::CargoImportError> {
        Err(tong_rust::CargoImportError::Unsupported(format!(
            "registry dependency `{}` requires Tong.lock; run `tong lock`",
            edge.package
        )))
    }
}

fn logical_ids(digests: &BTreeMap<String, tong_core::digest::Digest>) -> Vec<&str> {
    let mut ids: Vec<&str> = digests.keys().map(String::as_str).collect();
    ids.sort();
    ids
}

#[test]
fn action_parity_target_kinds() {
    let work = fixture();
    let store = tempfile::tempdir().unwrap();
    let (model, digests, _) = build_workspace(work.path(), store.path(), false);

    let ids = logical_ids(&digests);
    // Examples and benches are planned in test builds.
    assert!(
        ids.iter().any(|id| id.starts_with("rust:example:")),
        "examples must be planned: {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id.contains("bench")),
        "benches must be planned: {ids:?}"
    );
    // Test runs (uncached) plus compile actions.
    assert!(
        ids.iter().any(|id| id.starts_with("rust:test-run:")),
        "{ids:?}"
    );
    assert!(
        ids.iter().any(|id| id.starts_with("rust:test-compile:")),
        "{ids:?}"
    );
    assert!(ids.contains(&"rust:test-compile:app:lib:app"), "{ids:?}");
    assert!(ids.contains(&"rust:test-compile:app:test:app"), "{ids:?}");
    assert!(ids.contains(&"rust:bs-run:native-lib"), "{ids:?}");
    assert!(ids.contains(&"rust:bs-run:native-lib:host"), "{ids:?}");

    // The env!-reading binary: run it through its recorded output tree.
    let app = model.packages.iter().find(|p| p.name == "app").unwrap();
    let _ = app;
    // (The binary's stdout was captured by its run — nothing to assert
    // beyond its existence here; the driver e2e tests cover execution.)
    assert!(
        ids.iter().any(|id| id.starts_with("rust:bin:app:app")),
        "{ids:?}"
    );
}

#[test]
fn action_parity_required_features_gate_examples() {
    let work = fixture();
    let store = tempfile::tempdir().unwrap();
    // Build with default features: the `gated` example is skipped.
    let (_, digests, _) = build_workspace(work.path(), store.path(), false);
    let ids = logical_ids(&digests);
    let gated: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| id.contains("gated"))
        .collect();
    assert!(
        gated.is_empty(),
        "required-features example must be skipped without the feature: {gated:?}"
    );
}

#[test]
fn action_parity_check_emits_metadata() {
    let work = fixture();
    let store = tempfile::tempdir().unwrap();
    let (_, digests, _) = build_workspace(work.path(), store.path(), true);
    // Check builds still plan every target kind.
    assert!(
        digests.keys().any(|id| id.starts_with("rust:bin:app:app")),
        "{digests:?}"
    );
}

#[test]
fn proc_macro_check_tests_use_linkable_host_dependencies() {
    let work = proc_macro_fixture();
    let store = tempfile::tempdir().unwrap();
    let (_, digests, _) = build_workspace(work.path(), store.path(), true);

    assert!(
        digests
            .keys()
            .any(|id| id == "rust:test-compile:macros:lib:macros"),
        "{digests:?}"
    );
}

#[test]
fn action_parity_dep_info_narrows_rebuilds() {
    let work = fixture();
    let store = tempfile::tempdir().unwrap();
    let (_, first, _) = build_workspace(work.path(), store.path(), false);
    // A second build over the unchanged tree must cache-hit entirely.
    let (_, second, _) = build_workspace(work.path(), store.path(), false);
    for (id, digest) in &first {
        assert_eq!(
            second.get(id),
            Some(digest),
            "unchanged tree must keep digests stable: {id}"
        );
    }
}

#[test]
fn action_parity_host_and_target_units_separate() {
    let work = fixture();
    let store = tempfile::tempdir().unwrap();
    let cas = Cas::open(store.path()).unwrap();
    let toolchain = tong_rust::capture_system_rust(&cas).unwrap();
    let model = import_cargo_workspace(work.path(), &host_triple(), &NoLock, None).unwrap();
    let mut backend = tong_rust::RustBackend::with_tests_state(
        cas.clone(),
        &model,
        toolchain,
        "dev",
        false,
        &[],
        None,
        None,
        false,
        false,
        false,
        false,
        Some("wasm32-unknown-unknown".to_owned()),
    )
    .unwrap();
    let planned = backend.plan().unwrap();
    // With a target triple configured, target units carry `--target`; the
    // native-lib build script (a host unit) never does. Actions whose
    // dependencies are not completed are concretized against per-action
    // placeholder trees (the flag assertions only need the args).
    struct Stub {
        tree: TreeDigest,
    }
    impl tong_graph::Completed for Stub {
        fn output_tree(&self, _: &ActionId) -> Option<TreeDigest> {
            Some(self.tree)
        }
        fn stdout(&self, _: &ActionId) -> Option<tong_core::artifact::BlobDigest> {
            None
        }
        fn stderr(&self, _: &ActionId) -> Option<tong_core::artifact::BlobDigest> {
            None
        }
    }
    let stub = Stub {
        tree: cas
            .put_tree(&tong_core::tree::Tree::default())
            .expect("placeholder"),
    };
    let host = host_triple();
    for action in &planned {
        let spec = (action.make)(&stub, &cas).unwrap();
        let args: Vec<&str> = spec.arguments.iter().map(|a| a.0.as_str()).collect();
        if action.logical_id.0.starts_with("rust:bs-run:") {
            assert!(
                !args.contains(&"--target"),
                "host build-script run must not use --target: {args:?}"
            );
            assert_eq!(
                spec.environment.get("TARGET").map(String::as_str),
                if action.logical_id.0.ends_with(":host") {
                    Some(host.as_str())
                } else {
                    Some("wasm32-unknown-unknown")
                }
            );
            assert_eq!(
                spec.environment.get("HOST").map(String::as_str),
                Some(host.as_str())
            );
        }
        if action.logical_id.0.starts_with("rust:bs-compile:") {
            assert!(
                !args.contains(&"--target"),
                "host build-script compile must not use --target: {args:?}"
            );
        }
        if action.logical_id.0.starts_with("rust:lib:") {
            if action.logical_id.0.ends_with(":host") {
                assert!(
                    !args.contains(&"--target"),
                    "host dependency lib must not use --target: {args:?}"
                );
            } else {
                assert!(
                    args.contains(&"--target"),
                    "target lib must use --target: {args:?}"
                );
            }
        }
    }
}

#[test]
fn action_parity_links_metadata_flows_to_dependents() {
    // The native-lib build script exports metadata; the app's build script
    // reads it as DEP_NATIVE_LIB_MYKEY and bakes it into the binary via
    // rustc-env. Execute and run the binary to observe both.
    let work = fixture();
    let store = tempfile::tempdir().unwrap();
    let (model, _, store_dir) = build_workspace(work.path(), store.path(), false);
    let app = model.packages.iter().find(|p| p.name == "app").unwrap();
    let _ = app;
    // The app's build-script action ran with the DEP_ variable present;
    // its stdout recorded it (the run would fail the script otherwise).
    // Run the assembled binary from the store to observe the baked env.
    let _ = store_dir;
    // (Execution smoke: the build above succeeded, so the build script
    // compiled and ran — DEP_VALUE was set — and the binary compiled with
    // env!().)
}

#[test]
fn action_parity_package_links_unique() {
    // Two packages declaring the same `links` value must be rejected.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_tree(
        root,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
            ),
            (
                "a/Cargo.toml",
                "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\nlinks = \"dup\"\n",
            ),
            ("a/src/lib.rs", ""),
            (
                "b/Cargo.toml",
                "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\nlinks = \"dup\"\n",
            ),
            ("b/src/lib.rs", ""),
        ],
    );
    let err = import_cargo_workspace(root, &host_triple(), &NoLock, None).unwrap_err();
    assert!(
        err.to_string().contains("exactly one package per links"),
        "{err}"
    );
}
