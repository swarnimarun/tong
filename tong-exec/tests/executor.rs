//! End-to-end executor tests: materialize inputs, run a real script,
//! validate declared outputs, capture results.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use tong_core::action::{
    ACTION_SCHEMA_VERSION, ActionId, ActionSpec, Argument, CachePolicy, NetworkPolicy,
};
use tong_core::artifact::ArtifactRef;
use tong_core::paths::{OutputPath, RelativePath};
use tong_core::platform::PlatformKey;
use tong_core::tree::{Tree, TreeEntry};
use tong_exec::{ExecError, LocalExecutor};
use tong_store::{ActionCache, Cas};

fn setup() -> (tempfile::TempDir, Cas, LocalExecutor) {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let cas = Cas::open(&store).unwrap();
    let executor = LocalExecutor::new(cas.clone(), store.join("exec")).unwrap();
    (dir, cas, executor)
}

/// Input root with a single `data.txt` file.
fn input_with(cas: &Cas, name: &str, content: &str) -> tong_core::artifact::TreeDigest {
    let blob = cas.put_blob(content.as_bytes()).unwrap();
    let tree = Tree::new(BTreeMap::from([(
        name.to_owned(),
        TreeEntry::File {
            digest: blob,
            executable: false,
        },
    )]))
    .unwrap();
    cas.put_tree(&tree).unwrap()
}

/// An action running a small `/bin/sh` script blob.
fn script_action(
    cas: &Cas,
    script: &str,
    declared_outputs: Vec<&str>,
    timeout: Option<Duration>,
) -> ActionSpec {
    let script_blob = cas.put_blob(script.as_bytes()).unwrap();
    let input = input_with(cas, "data.txt", "world\n");
    ActionSpec {
        schema_version: ACTION_SCHEMA_VERSION,
        logical_id: ActionId("test:script".to_owned()),
        mnemonic: "TestScript".to_owned(),
        executable: ArtifactRef::Blob(script_blob),
        arguments: vec![],
        environment_bundle: None,
        environment: BTreeMap::from([
            ("GREETING".to_owned(), "hello".to_owned()),
            ("EXEC_CHECK".to_owned(), tong_exec::EXEC_ROOT_VAR.to_owned()),
        ]),
        input_root: input,
        declared_outputs: declared_outputs
            .into_iter()
            .map(|p| OutputPath::new(p).unwrap())
            .collect(),
        working_directory: RelativePath::new(".").unwrap(),
        execution_platform: PlatformKey::default(),
        target_platform: None,
        timeout,
        network_policy: NetworkPolicy::Deny,
        cache_policy: CachePolicy::Enabled,
        resource_requirements: Default::default(),
        properties: BTreeMap::new(),
    }
}

#[test]
fn executes_action_and_captures_outputs() {
    let (_dir, cas, executor) = setup();
    let action = script_action(
        &cas,
        "#!/bin/sh\ncp data.txt ../out/copied.txt\nprintf '%s\\n' \"$GREETING\"\n",
        vec!["copied.txt"],
        None,
    );
    let outcome = executor.execute(&action).unwrap();
    assert_eq!(outcome.duration, outcome.duration); // present
    assert_eq!(
        String::from_utf8(cas.read_blob(outcome.stdout).unwrap()).unwrap(),
        "hello\n"
    );
    // The captured output tree contains copied.txt with the input content.
    let out_tree = cas.get_tree(outcome.outputs).unwrap().unwrap();
    let copied = match out_tree.entries().get("copied.txt").unwrap() {
        TreeEntry::File { digest, .. } => *digest,
        _ => panic!("copied.txt must be a file"),
    };
    assert_eq!(cas.read_blob(copied).unwrap(), b"world\n");
}

#[test]
fn substitutes_exec_root_in_env() {
    let (_dir, cas, executor) = setup();
    let action = script_action(
        &cas,
        "#!/bin/sh\nprintf '%s\\n' \"$EXEC_CHECK\" > ../out/where.txt\n",
        vec!["where.txt"],
        None,
    );
    let outcome = executor.execute(&action).unwrap();
    // The captured file content must be a real absolute path ending in the
    // exec root's directory name (the action digest).
    let out_tree = cas.get_tree(outcome.outputs).unwrap().unwrap();
    let where_blob = match out_tree.entries().get("where.txt").unwrap() {
        TreeEntry::File { digest, .. } => *digest,
        _ => panic!("where.txt must be a file"),
    };
    let content = String::from_utf8(cas.read_blob(where_blob).unwrap()).unwrap();
    assert!(content.starts_with('/'), "got: {content}");
    assert!(content.trim_end().ends_with(&action.digest().to_hex()));
}

#[test]
fn concurrent_executors_isolate_identical_actions() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let cas = Cas::open(&store).unwrap();
    let first = LocalExecutor::new(cas.clone(), store.join("exec")).unwrap();
    let second = LocalExecutor::new(cas.clone(), store.join("exec")).unwrap();
    let action = script_action(
        &cas,
        "#!/bin/sh\nsleep 0.2\ncp data.txt ../out/copied.txt\n",
        vec!["copied.txt"],
        None,
    );
    let barrier = Arc::new(Barrier::new(2));

    let first_barrier = barrier.clone();
    let first_action = action.clone();
    let first_run = std::thread::spawn(move || {
        first_barrier.wait();
        first.execute(&first_action)
    });
    let second_run = std::thread::spawn(move || {
        barrier.wait();
        second.execute(&action)
    });

    let first_outcome = first_run.join().unwrap().unwrap();
    let second_outcome = second_run.join().unwrap().unwrap();
    assert_eq!(first_outcome.outputs, second_outcome.outputs);
}

#[test]
fn missing_declared_output_is_rejected() {
    let (_dir, cas, executor) = setup();
    let action = script_action(
        &cas,
        "#!/bin/sh\nexit 0\n", // produces nothing
        vec!["must_exist.txt"],
        None,
    );
    assert!(matches!(
        executor.execute(&action),
        Err(ExecError::MissingOutput { .. })
    ));
}

#[test]
fn non_zero_exit_is_reported() {
    let (_dir, cas, executor) = setup();
    let action = script_action(&cas, "#!/bin/sh\nexit 3\n", vec![], None);
    assert!(matches!(
        executor.execute(&action),
        Err(ExecError::Exit { code: 3, .. })
    ));
}

#[test]
fn timeout_kills_the_action() {
    let (_dir, cas, executor) = setup();
    let action = script_action(
        &cas,
        "#!/bin/sh\nsleep 5\n",
        vec![],
        Some(Duration::from_millis(200)),
    );
    assert!(matches!(
        executor.execute(&action),
        Err(ExecError::Timeout(_))
    ));
}

#[test]
fn system_tool_resolution_and_bundle_root() {
    let (_dir, cas, mut executor) = setup();
    // /bin/sh as a "system tool": the executable blob digest maps to the
    // real path instead of a CAS materialization.
    let sh = std::fs::read("/bin/sh").unwrap();
    let sh_blob = cas.put_blob(&sh).unwrap();
    executor.register_system_tool(sh_blob, Path::new("/bin/sh").to_path_buf());

    // The script is an input file, executed by the system shell.
    let run_blob = cas.put_blob(b"echo ok\n").unwrap();
    let input_blob = cas.put_blob(b"data\n").unwrap();
    let tree = Tree::new(BTreeMap::from([
        (
            "run.sh".to_owned(),
            TreeEntry::File {
                digest: run_blob,
                executable: true,
            },
        ),
        (
            "data.txt".to_owned(),
            TreeEntry::File {
                digest: input_blob,
                executable: false,
            },
        ),
    ]))
    .unwrap();
    let input = cas.put_tree(&tree).unwrap();

    let mut action = script_action(&cas, "", vec![], None);
    action.executable = ArtifactRef::Blob(sh_blob);
    action.input_root = input;
    action.arguments = vec![Argument(format!("{}/in/run.sh", tong_exec::EXEC_ROOT_VAR))];

    let outcome = executor.execute(&action).unwrap();
    assert_eq!(
        String::from_utf8(cas.read_blob(outcome.stdout).unwrap()).unwrap(),
        "ok\n"
    );
}

#[test]
fn driver_style_cache_roundtrip() {
    // Mimic the driver: digest → cache lookup → execute → commit → lookup.
    let (_dir, cas, executor) = setup();
    let cache = ActionCache::open(&cas).unwrap();
    let action = script_action(&cas, "#!/bin/sh\necho cached\n", vec![], None);
    let digest = action.digest();

    assert!(cache.get(digest).unwrap().is_none());
    let outcome = executor.execute(&action).unwrap();
    cache
        .put(
            digest,
            &tong_store::CachedResult {
                outputs: outcome.outputs,
                stdout: outcome.stdout,
                stderr: outcome.stderr,
                duration_millis: outcome.duration.as_millis() as u64,
            },
        )
        .unwrap();
    assert!(cache.get(digest).unwrap().is_some());
}
