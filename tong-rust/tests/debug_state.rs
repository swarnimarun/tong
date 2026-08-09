use tong_store::{StateStore, project_hash};

#[test]
fn debug_state_read() {
    let store = tong_store::Cas::open("/tmp/rerun-test/.tong/store").unwrap();
    let state = StateStore::open(std::path::Path::new("/tmp/rerun-test/.tong/store")).unwrap();
    let hash = project_hash(std::path::Path::new("/tmp/rerun-test")).unwrap();
    let manifest = state.latest(&hash).unwrap();
    let run = manifest
        .actions
        .iter()
        .find(|a| a.logical_id == "rust:bs-run:rerun-test")
        .unwrap();
    let bytes = store.read_blob(run.stdout).unwrap();
    eprintln!("stdout: {:?}", String::from_utf8_lossy(&bytes));
}
