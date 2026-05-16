use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub manifest_path: PathBuf,
    pub out_dir: PathBuf,
    pub profile: BuildProfile,
    pub verbose: bool,
    pub build_examples: bool,
    pub build_tests: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildProfile {
    Debug,
    Release,
}

impl BuildProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildOutput {
    pub artifacts: Vec<PathBuf>,
}

pub trait LanguageBackend<G> {
    fn name(&self) -> &'static str;
    fn build(&mut self, graph: &G, request: &BuildRequest) -> crate::error::Result<BuildOutput>;
}
