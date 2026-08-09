//! Adversarial sandbox tests (skipped when the platform sandbox binary is
//! absent — `bwrap` on Linux, `sandbox-exec` on macOS).

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::time::Duration;

use tong_core::action::{
    ACTION_SCHEMA_VERSION, ActionId, ActionSpec, Argument, CachePolicy, NetworkPolicy,
    ResourceRequirements,
};
use tong_core::artifact::{ArtifactRef, BlobDigest, TreeDigest};
use tong_core::paths::RelativePath;
use tong_core::platform::PlatformKey;
use tong_core::tree::Tree;
use tong_exec::sandbox::sandbox_available;
use tong_exec::{ExecError, LocalExecutor, SandboxLevel};

/// A sandboxed executor with /bin/sh registered as the system tool; the
/// empty input tree digest is stored in the CAS.
fn sandboxed(level: SandboxLevel) -> (tempfile::TempDir, LocalExecutor, TreeDigest) {
    let dir = tempfile::tempdir().unwrap();
    let cas = tong_store::Cas::open(dir.path().join("store")).unwrap();
    let tree = Tree::new(BTreeMap::new()).unwrap();
    let tree_digest = cas.put_tree(&tree).unwrap();
    let mut executor = LocalExecutor::with_sandbox(cas, dir.path().join("exec"), level).unwrap();
    let sh = std::fs::read("/bin/sh").unwrap();
    let digest = BlobDigest::new(tong_core::digest::Hasher::digest(&sh));
    executor.register_system_tool(digest, "/bin/sh".into());
    (dir, executor, tree_digest)
}

/// Runs `/bin/sh -c <script>`; returns Err on non-zero exit.
fn run_script(
    executor: &LocalExecutor,
    tree_digest: TreeDigest,
    script: &str,
) -> Result<(), ExecError> {
    let sh = std::fs::read("/bin/sh").unwrap();
    let digest = BlobDigest::new(tong_core::digest::Hasher::digest(&sh));
    let spec = ActionSpec {
        schema_version: ACTION_SCHEMA_VERSION,
        logical_id: ActionId("test".to_owned()),
        mnemonic: "Test".to_owned(),
        executable: ArtifactRef::Blob(digest),
        arguments: vec![Argument("-c".to_owned()), Argument(script.to_owned())],
        environment_bundle: None,
        environment: BTreeMap::new(),
        input_root: tree_digest,
        declared_outputs: Vec::new(),
        working_directory: RelativePath::new(".").unwrap(),
        execution_platform: PlatformKey::default(),
        target_platform: None,
        timeout: Some(Duration::from_secs(30)),
        network_policy: NetworkPolicy::Deny,
        cache_policy: CachePolicy::NoCache,
        resource_requirements: ResourceRequirements::default(),
        properties: BTreeMap::new(),
    };
    executor.execute(&spec).map(|_| ())
}

#[test]
fn l3_blocks_writes_outside_out_and_tmp() {
    if !sandbox_available(std::env::consts::OS) {
        eprintln!("skipping: no platform sandbox available");
        return;
    }
    let (_dir, exec, tree) = sandboxed(SandboxLevel::L3);
    let result = run_script(
        &exec,
        tree,
        "touch /etc/tong-sandbox-test 2>/dev/null && exit 0 || exit 1",
    );
    assert!(result.is_err(), "write outside out/tmp must be blocked");
    let _ = std::fs::remove_file("/etc/tong-sandbox-test");
    let (_dir, control, tree) = sandboxed(SandboxLevel::L2);
    let result = run_script(
        &control,
        tree,
        "touch /etc/tong-sandbox-test 2>/dev/null && exit 0 || exit 1",
    );
    let _ = std::fs::remove_file("/etc/tong-sandbox-test");
    assert!(result.is_ok(), "unsandboxed control must succeed");
}

#[test]
fn l3_blocks_reading_the_real_home() {
    if !sandbox_available(std::env::consts::OS) {
        eprintln!("skipping: no platform sandbox available");
        return;
    }
    let real_home = std::env::var("HOME").unwrap_or_else(|_| "/nonexistent".to_owned());
    let (_dir, exec, tree) = sandboxed(SandboxLevel::L3);
    let script = format!("ls {real_home} >/dev/null 2>&1 && exit 0 || exit 1");
    assert!(
        run_script(&exec, tree, &script).is_err(),
        "reading the real home must be blocked"
    );
    let (_dir, control, tree) = sandboxed(SandboxLevel::L2);
    assert!(
        run_script(&control, tree, &script).is_ok(),
        "control must succeed"
    );
}

#[test]
fn l4_blocks_network_connections() {
    if !sandbox_available(std::env::consts::OS) {
        eprintln!("skipping: no platform sandbox available");
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let script = format!(
        "python3 -c \"import socket; s=socket.socket(); s.settimeout(5); \
         s.connect(('127.0.0.1', {port}))\" 2>/dev/null && exit 0 || exit 1"
    );
    let (_dir, exec, tree) = sandboxed(SandboxLevel::L4);
    assert!(
        run_script(&exec, tree, &script).is_err(),
        "network access at L4 must be blocked"
    );
}

#[test]
fn l3_allows_normal_execution() {
    if !sandbox_available(std::env::consts::OS) {
        eprintln!("skipping: no platform sandbox available");
        return;
    }
    let (_dir, exec, tree) = sandboxed(SandboxLevel::L3);
    let result = run_script(&exec, tree, "echo hello > /dev/null && exit 0 || exit 1");
    assert!(result.is_ok(), "a benign command must succeed under L3");
}

#[test]
fn l3_allows_loopback_connections() {
    if !sandbox_available(std::env::consts::OS) {
        eprintln!("skipping: no platform sandbox available");
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let script = format!(
        "python3 -c \"import socket; s=socket.socket(); s.settimeout(5); \
         s.connect(('127.0.0.1', {port}))\" 2>/dev/null && exit 0 || exit 1"
    );
    let (_dir, exec, tree) = sandboxed(SandboxLevel::L3);
    assert!(
        run_script(&exec, tree, &script).is_ok(),
        "loopback must be reachable at L3"
    );
}
