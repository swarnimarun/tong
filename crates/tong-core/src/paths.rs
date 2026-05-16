use crate::error::{IoContext, Result, TongError};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub fn canonicalize(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).with_context(format!("failed to canonicalize {}", path.display()))
}

pub fn normalize_crate_name(name: &str) -> String {
    name.replace('-', "_")
}

pub fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

pub fn find_program(name: &str) -> Result<PathBuf> {
    find_program_uncanonicalized(name).and_then(|path| canonicalize(&path))
}

pub fn find_program_uncanonicalized(name: &str) -> Result<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return Ok(candidate.to_path_buf());
    }

    let path = env::var_os("PATH")
        .ok_or_else(|| TongError::unsupported(format!("PATH is not set; cannot find `{name}`")))?;

    for dir in env::split_paths(&path) {
        let full = dir.join(name);
        if is_executable_candidate(&full) {
            return Ok(full);
        }

        if cfg!(windows) {
            let exe = dir.join(format!("{name}.exe"));
            if is_executable_candidate(&exe) {
                return Ok(exe);
            }
        }
    }

    Err(TongError::unsupported(format!(
        "could not find `{name}` in PATH"
    )))
}

fn is_executable_candidate(path: &Path) -> bool {
    path.is_file()
}

pub fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
