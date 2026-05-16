use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestKind {
    Cargo,
    Tong,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub path: PathBuf,
    pub root: PathBuf,
    pub kind: ManifestKind,
    pub package: Package,
    pub features: BTreeMap<String, Vec<String>>,
    pub sources: BTreeMap<String, SourceSpec>,
    pub build_script: Option<PathBuf>,
    pub lib: Option<LibTarget>,
    pub bins: Vec<BinTarget>,
    pub tests: Vec<TestTarget>,
    pub examples: Vec<ExampleTarget>,
    pub dependencies: Vec<Dependency>,
    pub build_dependencies: Vec<Dependency>,
    pub workspace: Option<Workspace>,
}

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub edition: String,
}

#[derive(Debug, Clone)]
pub struct LibTarget {
    pub name: String,
    pub path: PathBuf,
    pub proc_macro: bool,
}

#[derive(Debug, Clone)]
pub struct BinTarget {
    pub name: String,
    pub path: PathBuf,
    pub required_features: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TestTarget {
    pub name: String,
    pub path: PathBuf,
    pub required_features: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExampleTarget {
    pub name: String,
    pub path: PathBuf,
    pub required_features: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Dependency {
    pub alias: String,
    pub package: String,
    pub features: Vec<String>,
    pub default_features: bool,
    pub optional: bool,
    pub source: DependencySource,
}

#[derive(Debug, Clone)]
pub enum DependencySource {
    Path(PathBuf),
    Registry { version: String },
    Source(SourceSpec),
}

#[derive(Debug, Clone)]
pub enum SourceSpec {
    Git {
        url: String,
        rev: Option<String>,
        subdir: Option<String>,
    },
    Tar {
        url: String,
        sha256: Option<String>,
        strip_prefix: Option<String>,
        subdir: Option<String>,
    },
    Zip {
        url: String,
        sha256: Option<String>,
        strip_prefix: Option<String>,
        subdir: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub members: Vec<PathBuf>,
    pub resolver: Option<String>,
    pub dependencies: BTreeMap<String, Dependency>,
}
