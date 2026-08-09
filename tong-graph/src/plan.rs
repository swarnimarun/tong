//! Planned actions and the build scheduler.
//!
//! Analysis produces [`PlannedAction`]s whose inputs may reference the
//! outputs of other planned actions — dependency artifacts whose digests are
//! only known once their producers have run (PLAN.md section 8.3: never
//! cache under a key created before the complete input set is known).
//! Each planned action therefore carries a *concretization closure* that
//! assembles the final [`ActionSpec`] — including the input root — from the
//! completed dependency results at schedule time.

use std::collections::BTreeMap;
use std::fmt;
use std::io;

use tong_core::action::{ActionId, ActionSpec};
use tong_core::artifact::{BlobDigest, TreeDigest};
use tong_store::Cas;

/// View of the actions completed so far, available to concretization
/// closures.
pub trait Completed {
    /// The captured output tree of a completed action.
    fn output_tree(&self, action: &ActionId) -> Option<TreeDigest>;
    /// The captured stdout blob of a completed action.
    fn stdout(&self, action: &ActionId) -> Option<BlobDigest>;
    /// The captured stderr blob of a completed action.
    fn stderr(&self, action: &ActionId) -> Option<BlobDigest>;
}

/// Concretization function: assembles the final spec from completed deps.
pub type MakeSpec = Box<dyn Fn(&dyn Completed, &Cas) -> Result<ActionSpec, PlanError> + Send>;

/// An action whose spec is concretized once its dependencies complete.
pub struct PlannedAction {
    /// Graph identity (diagnostics, dependency edges).
    pub logical_id: ActionId,
    /// Diagnostic mnemonic.
    pub mnemonic: String,
    /// Actions that must complete first.
    pub deps: Vec<ActionId>,
    /// Whether the action belongs to a package outside the workspace
    /// (registry, git, or path dependencies). `tong build --deps-only`
    /// executes only external actions, so docker dep layers bust only when
    /// the lockfile or toolchain changes.
    pub external: bool,
    /// Assembles the final `ActionSpec` from completed dependencies.
    pub make: MakeSpec,
}

impl fmt::Debug for PlannedAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlannedAction")
            .field("logical_id", &self.logical_id)
            .field("mnemonic", &self.mnemonic)
            .field("deps", &self.deps)
            .finish_non_exhaustive()
    }
}

/// Planning or concretization failure.
#[derive(Debug)]
pub enum PlanError {
    /// A dependency action never completed (scheduler invariant broken).
    MissingDependency(ActionId),
    /// I/O failure (store access, manifest reading, toolchain capture).
    Io(io::Error),
    /// Backend-level failure with a message.
    Message(String),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDependency(id) => {
                write!(f, "dependency {} did not complete", id.0)
            }
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Message(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<io::Error> for PlanError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<&str> for PlanError {
    fn from(msg: &str) -> Self {
        Self::Message(msg.to_owned())
    }
}

/// A detected cycle in the action graph.
#[derive(Debug)]
pub struct CycleError {
    /// Actions still in the graph when the cycle was found.
    pub remaining: Vec<ActionId>,
}

impl fmt::Display for CycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cycle detected among actions: {:?}", self.remaining)
    }
}

impl std::error::Error for CycleError {}

/// Returns actions in a valid execution order (dependencies first), or the
/// cycle if the graph is not a DAG.
pub fn topological_order(actions: &[PlannedAction]) -> Result<Vec<&PlannedAction>, CycleError> {
    let mut by_id = BTreeMap::new();
    for action in actions {
        by_id.insert(&action.logical_id, action);
    }
    // Every dep edge must reference a known action.
    let mut indegree: BTreeMap<&ActionId, usize> = actions
        .iter()
        .map(|action| (&action.logical_id, action.deps.len()))
        .collect();
    let mut dependents: BTreeMap<&ActionId, Vec<&ActionId>> = BTreeMap::new();
    for action in actions {
        for dep in &action.deps {
            if !by_id.contains_key(dep) {
                // Unknown dependency: treat as an error via cycle report.
                continue;
            }
            dependents.entry(dep).or_default().push(&action.logical_id);
        }
    }

    let mut ready: Vec<&PlannedAction> = actions
        .iter()
        .filter(|action| indegree[&action.logical_id] == 0)
        .collect();
    ready.sort_by_key(|action| action.logical_id.0.clone());

    let mut order = Vec::with_capacity(actions.len());
    while let Some(action) = ready.pop() {
        order.push(action);
        if let Some(children) = dependents.remove(&action.logical_id) {
            for child in children {
                let degree = indegree.get_mut(child).unwrap();
                *degree -= 1;
                if *degree == 0 {
                    let node = by_id[child];
                    ready.push(node);
                    ready.sort_by_key(|action| action.logical_id.0.clone());
                }
            }
        }
    }

    if order.len() != actions.len() {
        let remaining: Vec<ActionId> = by_id
            .iter()
            .filter(|(id, _)| !order.iter().any(|a| &a.logical_id == **id))
            .map(|(id, _)| (*id).clone())
            .collect();
        return Err(CycleError { remaining });
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tong_core::action::{
        ACTION_SCHEMA_VERSION, ActionSpec, Argument, CachePolicy, NetworkPolicy,
    };
    use tong_core::paths::RelativePath;
    use tong_core::platform::PlatformKey;
    use tong_core::tree::Tree;

    fn planned(name: &str, deps: &[&str]) -> PlannedAction {
        let name = name.to_owned();
        let deps: Vec<ActionId> = deps.iter().map(|d| ActionId((*d).to_owned())).collect();
        let logical_id = ActionId(name.clone());
        let deps_for_make = deps.clone();
        PlannedAction {
            logical_id,
            mnemonic: "Test".to_owned(),
            deps,
            external: false,
            make: Box::new(move |completed, cas| {
                let mut inputs = vec![(RelativePath::new(".").unwrap(), {
                    let tree = Tree::default();
                    cas.put_tree(&tree)?
                })];
                for dep in &deps_for_make {
                    let tree = completed
                        .output_tree(dep)
                        .ok_or_else(|| PlanError::MissingDependency(dep.clone()))?;
                    inputs.push((RelativePath::new("deps").unwrap(), tree));
                }
                let root = cas.assemble(&inputs)?;
                Ok(ActionSpec {
                    schema_version: ACTION_SCHEMA_VERSION,
                    logical_id: ActionId(name.clone()),
                    mnemonic: "Test".to_owned(),
                    executable: tong_core::artifact::ArtifactRef::Blob(
                        cas.put_blob(b"tool").unwrap(),
                    ),
                    arguments: vec![Argument("--x".to_owned())],
                    environment_bundle: None,
                    environment: Default::default(),
                    input_root: root,
                    declared_outputs: vec![],
                    working_directory: RelativePath::new(".").unwrap(),
                    execution_platform: PlatformKey::default(),
                    target_platform: None,
                    timeout: None,
                    network_policy: NetworkPolicy::Deny,
                    cache_policy: CachePolicy::Enabled,
                    resource_requirements: Default::default(),
                    properties: Default::default(),
                })
            }),
        }
    }

    #[test]
    fn schedules_in_dependency_order() {
        let actions = vec![
            planned("bin", &["lib"]),
            planned("lib", &["dep"]),
            planned("dep", &[]),
        ];
        let order = topological_order(&actions).unwrap();
        let names: Vec<&str> = order.iter().map(|a| a.logical_id.0.as_str()).collect();
        assert_eq!(names, vec!["dep", "lib", "bin"]);
    }

    #[test]
    fn detects_cycles() {
        let actions = vec![
            planned("a", &["b"]),
            planned("b", &["c"]),
            planned("c", &["a"]),
        ];
        let err = topological_order(&actions).unwrap_err();
        assert_eq!(err.remaining.len(), 3);
    }
}
