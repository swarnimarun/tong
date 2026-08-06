//! Golden digest vectors (PLAN.md sections 4.5 and 16, Phase 0 exit
//! criteria).
//!
//! These tests pin the canonical encoding: any platform, any machine, and
//! any future refactor must produce these exact digests for these fixtures.
//! If a vector changes, the canonical encoding changed — that is a
//! cache-invalidation event and requires a schema-version bump, never a
//! silent test update.

use std::collections::BTreeMap;
use std::time::Duration;

use tong_core::action::{
    ACTION_SCHEMA_VERSION, ActionId, ActionSpec, Argument, CachePolicy, CanonicalValue,
    NetworkPolicy, ResourceRequirements,
};
use tong_core::artifact::{ArtifactRef, BlobDigest, TreeDigest};
use tong_core::bundle::{BundleRef, EnvironmentBundle};
use tong_core::canonical;
use tong_core::digest::Hasher;
use tong_core::paths::{OutputPath, RelativePath};
use tong_core::platform::PlatformKey;
use tong_core::tree::{Tree, TreeEntry};

const PLATFORM_DIGEST: &str = "c1cc9dfa421076637cd23e7c617ef904648313b90a6702e845b19645dce415ac";
const TREE_DIGEST: &str = "c120daa38f95f863000462ee978b361d5fe5452993a5be32a0e2e1940d651b91";
const BUNDLE_DIGEST: &str = "1bf33c37f690b0917b5a8ae8b3666b05cc798712c8f97c765c531b0b1d2a06c4";
const ACTION_DIGEST: &str = "5a3c952a0202d8cf4c8094c056788e2474cc0fce0c6c5fd7cf1927e8600cb217";

fn blob(content: &str) -> BlobDigest {
    BlobDigest::new(Hasher::digest(content.as_bytes()))
}

fn linux_x86_64() -> PlatformKey {
    PlatformKey::from_pairs(&[("os", "linux"), ("arch", "x86_64"), ("abi", "gnu")])
}

fn sample_tree() -> Tree {
    let codec = Tree::new(BTreeMap::from([
        (
            "codec.c".to_owned(),
            TreeEntry::File {
                digest: blob("int main(void) {}"),
                executable: false,
            },
        ),
        (
            "codec.h".to_owned(),
            TreeEntry::File {
                digest: blob("#pragma once"),
                executable: false,
            },
        ),
    ]))
    .unwrap();
    Tree::new(BTreeMap::from([
        (
            "tong".to_owned(),
            TreeEntry::File {
                digest: blob("#!/bin/sh\n"),
                executable: true,
            },
        ),
        (
            "VERSION".to_owned(),
            TreeEntry::Symlink {
                target: "codec/codec.c".to_owned(),
            },
        ),
        ("src".to_owned(), TreeEntry::Directory(codec.digest())),
    ]))
    .unwrap()
}

fn sample_bundle() -> EnvironmentBundle {
    EnvironmentBundle {
        name: "rust-1.97.1-linux-x86_64".to_owned(),
        provider: "download".to_owned(),
        platform: linux_x86_64(),
        variables: BTreeMap::from([
            ("RUSTC".to_owned(), "bin/rustc".to_owned()),
            ("CARGO".to_owned(), "bin/cargo".to_owned()),
        ]),
        files: sample_tree().digest(),
        metadata: BTreeMap::from([(
            "tong.rust.rustc_verbose_version".to_owned(),
            CanonicalValue::String("rustc 1.97.1".to_owned()),
        )]),
    }
}

fn sample_action(input_root: TreeDigest, bundle: Option<&EnvironmentBundle>) -> ActionSpec {
    ActionSpec {
        schema_version: ACTION_SCHEMA_VERSION,
        logical_id: ActionId("//crates/codec:codec".to_owned()),
        mnemonic: "RustCompile".to_owned(),
        executable: ArtifactRef::Blob(blob("fake-rustc")),
        arguments: vec![
            Argument("--crate-name".to_owned()),
            Argument("codec".to_owned()),
        ],
        environment_bundle: bundle.map(BundleRef::of),
        environment: BTreeMap::from([("CARGO_PKG_VERSION".to_owned(), "0.1.0".to_owned())]),
        input_root,
        declared_outputs: vec![OutputPath::new("lib/libcodec.rlib").unwrap()],
        working_directory: RelativePath::new(".").unwrap(),
        execution_platform: linux_x86_64(),
        target_platform: None,
        timeout: Some(Duration::from_secs(60)),
        network_policy: NetworkPolicy::Deny,
        cache_policy: CachePolicy::Enabled,
        resource_requirements: ResourceRequirements {
            cpu_cores: Some(4),
            memory_bytes: None,
            disk_bytes: None,
        },
        properties: BTreeMap::from([(
            "tong.rust.profile".to_owned(),
            CanonicalValue::String("release".to_owned()),
        )]),
    }
}

#[test]
fn platform_key_matches_golden_vector() {
    assert_eq!(
        canonical::digest_of(&linux_x86_64()).to_hex(),
        PLATFORM_DIGEST
    );
}

#[test]
fn tree_matches_golden_vector() {
    assert_eq!(sample_tree().digest().digest().to_hex(), TREE_DIGEST);
}

#[test]
fn environment_bundle_matches_golden_vector() {
    assert_eq!(sample_bundle().digest().to_hex(), BUNDLE_DIGEST);
}

#[test]
fn action_matches_golden_vector() {
    let action = sample_action(sample_tree().digest(), Some(&sample_bundle()));
    assert_eq!(action.digest().to_hex(), ACTION_DIGEST);
}

/// PLAN.md Phase 0 exit criterion: logical target renames do not alter
/// action digests.
#[test]
fn renaming_an_action_does_not_change_its_digest() {
    let original = sample_action(sample_tree().digest(), Some(&sample_bundle()));
    let mut renamed = original.clone();
    renamed.logical_id = ActionId("//renamed/workspace:elsewhere".to_owned());
    renamed.mnemonic = "SomethingElse".to_owned();
    assert_eq!(original.digest(), renamed.digest());
}

/// Canonical map ordering: construction order never affects the digest
/// (PLAN.md section 4.5).
#[test]
fn map_insertion_order_does_not_change_the_digest() {
    let mut action = sample_action(sample_tree().digest(), Some(&sample_bundle()));
    action.environment = BTreeMap::from([
        ("ZEBRA".to_owned(), "1".to_owned()),
        ("APPLE".to_owned(), "2".to_owned()),
    ]);
    let reference = action.digest();

    let mut reordered = sample_action(sample_tree().digest(), Some(&sample_bundle()));
    let mut environment = BTreeMap::new();
    environment.insert("APPLE".to_owned(), "2".to_owned());
    environment.insert("ZEBRA".to_owned(), "1".to_owned());
    reordered.environment = environment;

    assert_eq!(reference, reordered.digest());
}

/// The semantic digest reacts to every execution-relevant change.
#[test]
fn semantic_changes_change_the_digest() {
    let base = sample_action(sample_tree().digest(), Some(&sample_bundle()));
    let base_digest = base.digest();

    let mut no_bundle = base.clone();
    no_bundle.environment_bundle = None;
    assert_ne!(base_digest, no_bundle.digest());

    let mut network = base.clone();
    network.network_policy = NetworkPolicy::Allow;
    assert_ne!(base_digest, network.digest());

    let mut not_cacheable = base.clone();
    not_cacheable.cache_policy = CachePolicy::Disabled;
    assert_ne!(base_digest, not_cacheable.digest());

    let mut other_schema = base.clone();
    other_schema.schema_version = ACTION_SCHEMA_VERSION + 1;
    assert_ne!(base_digest, other_schema.digest());
}
