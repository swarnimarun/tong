use std::fmt::{self, Display};
use std::io;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, TongError>;

#[derive(Debug)]
pub enum TongError {
    Io {
        context: String,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        line: usize,
        message: String,
    },
    InvalidManifest {
        path: PathBuf,
        message: String,
    },
    Unsupported {
        message: String,
    },
    Cycle {
        package: String,
    },
    CommandFailed {
        program: PathBuf,
        status: String,
        stderr: String,
    },
}

impl TongError {
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    pub fn parse(path: PathBuf, line: usize, message: impl Into<String>) -> Self {
        Self::Parse {
            path,
            line,
            message: message.into(),
        }
    }

    pub fn invalid_manifest(path: PathBuf, message: impl Into<String>) -> Self {
        Self::InvalidManifest {
            path,
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported {
            message: message.into(),
        }
    }
}

impl Display for TongError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, .. } => write!(f, "{context}"),
            Self::Parse {
                path,
                line,
                message,
            } => write!(f, "{}:{line}: {message}", path.display()),
            Self::InvalidManifest { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
            Self::Unsupported { message } => write!(f, "unsupported: {message}"),
            Self::Cycle { package } => write!(f, "dependency cycle involving package `{package}`"),
            Self::CommandFailed {
                program,
                status,
                stderr,
            } => {
                if stderr.trim().is_empty() {
                    write!(f, "{} failed with {status}", program.display())
                } else {
                    write!(
                        f,
                        "{} failed with {status}\n{}",
                        program.display(),
                        stderr.trim_end()
                    )
                }
            }
        }
    }
}

impl std::error::Error for TongError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub trait IoContext<T> {
    fn with_context(self, context: impl Into<String>) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn with_context(self, context: impl Into<String>) -> Result<T> {
        self.map_err(|source| TongError::io(context, source))
    }
}
