//! Registry configuration and the HTTP/file fetch client.
//!
//! A registry is an index URL plus a download template. The default is
//! crates.io's sparse index (`sparse+https://index.crates.io/`); `file://`
//! registries are first-class (tests, offline mirrors). Network happens
//! only in `tong lock` / `tong fetch`; builds are always offline.
//!
//! The `dl` template may contain the markers `{crate}`, `{version}`,
//! `{prefix}`, `{lowerprefix}`, `{sha256-checksum}` (Cargo book rules);
//! when none are present, `/{crate}/{version}/download` is appended.

use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

/// Size cap for index files (config.json, per-crate index entries).
pub const INDEX_SIZE_LIMIT: usize = 64 * 1024 * 1024;
/// Size cap for `.crate` archives.
pub const CRATE_SIZE_LIMIT: usize = 512 * 1024 * 1024;

/// A configured registry: index URL and download template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryConfig {
    /// Index base URL (`sparse+` prefix stripped).
    pub index_url: String,
    /// Download template (markers or a base that gets the default suffix).
    pub dl: String,
    /// Optional API base URL (unused by tong itself).
    pub api: Option<String>,
}

impl RegistryConfig {
    /// The default crates.io registry. The `dl` template is fetched from
    /// the index's `config.json` on first use.
    pub fn crates_io() -> Self {
        Self {
            index_url: "https://index.crates.io/".to_owned(),
            dl: String::new(),
            api: None,
        }
    }

    /// Builds a registry from an index URL (`sparse+https://…`,
    /// `https://…`, or `file://…`), fetching `<index>/config.json` for the
    /// download template.
    pub fn from_url(index: &str) -> Result<Self, FetchError> {
        let index_url = index
            .strip_prefix("sparse+")
            .unwrap_or(index)
            .trim_end_matches('/')
            .to_owned();
        let config_url = format!("{index_url}/config.json");
        let bytes = fetch_url(&config_url, INDEX_SIZE_LIMIT)
            .map_err(|err| FetchError::BadConfig(format!("{config_url}: {err}")))?;
        let config: IndexConfig = serde_json::from_slice(&bytes)
            .map_err(|err| FetchError::BadConfig(format!("{config_url}: {err}")))?;
        Ok(Self {
            index_url,
            dl: config.dl.unwrap_or_default(),
            api: config.api,
        })
    }

    /// Substitutes the download markers into the `dl` template; a
    /// marker-less `dl` (the modern crates.io config:
    /// `https://static.crates.io/crates`) uses the static layout
    /// `{dl}/{crate}/{crate}-{version}.crate`; an empty `dl` falls back to
    /// the legacy index download path.
    pub fn download_url(&self, name: &str, version: &semver::Version, checksum: &str) -> String {
        let lower = name.to_ascii_lowercase();
        let (prefix, lowerprefix) = match lower.len() {
            1 => ("1".to_owned(), "1".to_owned()),
            2 => ("2".to_owned(), "2".to_owned()),
            3 => (lower[..1].to_owned(), lower[..1].to_owned()),
            _ => (lower[..2].to_owned(), lower[..2].to_owned()),
        };
        let mut url = self.dl.clone();
        if url.contains('{') {
            url = url
                .replace("{crate}", name)
                .replace("{version}", &version.to_string())
                .replace("{prefix}", &prefix)
                .replace("{lowerprefix}", &lowerprefix)
                .replace("{sha256-checksum}", checksum);
        } else if self.dl.is_empty() {
            url = format!(
                "{}/crate/{name}/{version}/download",
                self.index_url.trim_end_matches('/')
            );
        } else {
            url = format!(
                "{}/{lower}/{lower}-{version}.crate",
                self.dl.trim_end_matches('/')
            );
        }
        url
    }
}

/// `config.json` of an index.
#[derive(Deserialize)]
struct IndexConfig {
    dl: Option<String>,
    api: Option<String>,
}

/// Fetch failure with an actionable message.
#[derive(Debug)]
pub enum FetchError {
    /// HTTP failure (status or transport).
    Http(String),
    /// Filesystem failure.
    Io(io::Error),
    /// Response exceeded the size cap.
    SizeLimit(String),
    /// Invalid registry configuration or index content.
    BadConfig(String),
    /// The resource does not exist (404 / missing file).
    NotFound(String),
    /// A downloaded `.crate` did not match its index checksum.
    Checksum {
        /// Package name.
        package: String,
        /// Expected checksum (from the index).
        expected: String,
        /// Actual checksum of the downloaded archive.
        got: String,
    },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(msg) => write!(f, "{msg}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::SizeLimit(what) => write!(f, "{what} exceeds the size limit"),
            Self::BadConfig(msg) => write!(f, "invalid registry configuration: {msg}"),
            Self::NotFound(what) => write!(f, "{what} not found"),
            Self::Checksum {
                package,
                expected,
                got,
            } => write!(
                f,
                "checksum mismatch for `{package}`: expected {expected}, got {got}; \
                 the download is corrupt or the index is out of date"
            ),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<io::Error> for FetchError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Fetches a URL: `file://` reads the local path; `http(s)://` uses ureq.
/// Responses are size-capped.
/// Builds the HTTP agent for index and crate downloads: bounded
/// connect/overall timeouts so a stuck server cannot hang `tong lock`
/// forever (the default agent has no timeout and a dead connection
/// blocks indefinitely).
pub(crate) fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .build()
        .new_agent()
}

pub fn fetch_url(url: &str, max_size: usize) -> Result<Vec<u8>, FetchError> {
    if let Some(path) = url.strip_prefix("file://") {
        let path = Path::new(path);
        if !path.is_file() {
            return Err(FetchError::NotFound(path.display().to_string()));
        }
        let len = fs::metadata(path)?.len() as usize;
        if len > max_size {
            return Err(FetchError::SizeLimit(format!(
                "{} ({} bytes)",
                path.display(),
                len
            )));
        }
        return fs::read(path).map_err(FetchError::from);
    }
    if let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    {
        let _ = rest;
    } else {
        return Err(FetchError::BadConfig(format!(
            "unsupported URL scheme in {url:?} (expected http://, https://, or file://)"
        )));
    }
    let agent = http_agent();
    let t_fetch = std::time::Instant::now();
    let mut response = agent.get(url).call().map_err(|err| match err {
        ureq::Error::StatusCode(404) => FetchError::NotFound(url.to_owned()),
        other => FetchError::Http(format!("GET {url}: {other}")),
    })?;
    let body = response
        .body_mut()
        .with_config()
        .limit(max_size as u64)
        .read_to_vec()
        .map_err(|err| match err {
            ureq::Error::Io(io_err) if io_err.kind() == io::ErrorKind::UnexpectedEof => {
                FetchError::SizeLimit(url.to_owned())
            }
            other => FetchError::Http(format!("GET {url}: {other}")),
        })?;
    tracing::debug!(
        target: "tong::fetch",
        url,
        status = response.status().as_u16(),
        bytes = body.len(),
        duration_ms = t_fetch.elapsed().as_millis() as u64,
    );
    Ok(body)
}

/// Convenience: downloads with the crate archive size cap.
pub fn fetch_crate_bytes(url: &str) -> Result<Vec<u8>, FetchError> {
    fetch_url(url, CRATE_SIZE_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls_read_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        fs::write(&path, b"{}").unwrap();
        let url = format!("file://{}", path.display());
        assert_eq!(fetch_url(&url, 1024).unwrap(), b"{}");
        assert!(matches!(
            fetch_url(&format!("{url}-missing"), 1024),
            Err(FetchError::NotFound(_))
        ));
    }

    #[test]
    fn download_url_substitutes_markers() {
        let config = RegistryConfig {
            index_url: "https://index.crates.io/".to_owned(),
            dl: "https://static.crates.io/crates/{crate}/{crate}-{version}.crate".to_owned(),
            api: None,
        };
        let version = semver::Version::new(1, 2, 3);
        assert_eq!(
            config.download_url("serde", &version, "abc"),
            "https://static.crates.io/crates/serde/serde-1.2.3.crate"
        );
    }

    #[test]
    fn download_url_defaults_to_cargo_layout() {
        let config = RegistryConfig {
            index_url: "https://index.crates.io/".to_owned(),
            dl: String::new(),
            api: None,
        };
        let version = semver::Version::new(1, 2, 3);
        assert_eq!(
            config.download_url("serde", &version, "abc"),
            "https://index.crates.io/crate/serde/1.2.3/download"
        );
    }

    #[test]
    fn rejects_unknown_schemes() {
        assert!(matches!(
            fetch_url("ftp://example.com/x", 1024),
            Err(FetchError::BadConfig(_))
        ));
    }

    #[test]
    fn size_limit_rejects_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        fs::write(&path, vec![0u8; 4096]).unwrap();
        let url = format!("file://{}", path.display());
        assert!(matches!(
            fetch_url(&url, 1024),
            Err(FetchError::SizeLimit(_))
        ));
    }
}
