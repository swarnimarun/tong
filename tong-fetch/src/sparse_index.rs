//! Sparse registry index client.
//!
//! Fetches per-crate index entries (`<index>/<prefix>/<name>` per the Cargo
//! book path rules), cached under the store's `index/` directory with
//! ETag-based revalidation. The cache is a plain mutable cache (the index
//! is not CAS content); a network failure with a valid cache falls back to
//! the cache with a warning, matching Cargo's offline resilience.
//!
//! `file://` indexes are fetched fresh every time (no ETags); they serve
//! tests and offline mirrors.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use semver::{Version, VersionReq};
use serde::Deserialize;

use crate::registry::{FetchError, RegistryConfig, fetch_url};

/// One version entry of a crate's index file.
#[derive(Clone, Debug, Deserialize)]
pub struct IndexVersion {
    /// Package name.
    pub name: String,
    /// Version.
    pub vers: Version,
    /// Dependency edges.
    #[serde(default)]
    pub deps: Vec<IndexDep>,
    /// `.crate` archive SHA-256 (hex).
    pub cksum: String,
    /// Declared features.
    #[serde(default)]
    pub features: BTreeMap<String, Vec<String>>,
    /// Resolver-v2 feature data (merged into `features`).
    #[serde(default)]
    pub features2: Option<BTreeMap<String, Vec<String>>>,
    /// Yanked versions are not selected unless already in the lockfile.
    #[serde(default)]
    pub yanked: bool,
    /// Minimum supported Rust version.
    pub rust_version: Option<String>,
    /// Index schema version; entries with `v > 2` are skipped (Cargo's
    /// forward-compat rule).
    #[serde(default)]
    pub v: u32,
}

/// A dependency edge inside an index entry.
#[derive(Clone, Debug, Deserialize)]
pub struct IndexDep {
    /// Dependency name (extern name).
    pub name: String,
    /// Version requirement.
    pub req: VersionReq,
    /// Features requested on the dependency.
    #[serde(default)]
    pub features: Vec<String>,
    /// Optional dependency.
    #[serde(default)]
    pub optional: bool,
    /// Whether the dependency's default feature is enabled.
    #[serde(default = "default_true")]
    pub default_features: bool,
    /// Target-specific dependency (`cfg(...)` expression).
    pub target: Option<String>,
    /// Dependency kind.
    #[serde(default)]
    pub kind: IndexDepKind,
    /// Real package name when the dependency is renamed.
    pub package: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Dependency kind in the index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexDepKind {
    /// `[dependencies]`.
    #[default]
    Normal,
    /// `[dev-dependencies]`.
    Dev,
    /// `[build-dependencies]`.
    Build,
}

/// Cached sparse index client.
#[derive(Clone, Debug)]
pub struct IndexClient {
    cache_dir: PathBuf,
    /// The registry this client serves.
    config: RegistryConfig,
    /// Never touch the network (`tong lock --offline`); a missing cache
    /// entry is an error.
    offline: bool,
    /// Names already reported as fetched (progress dedup: the resolver
    /// may ask for one crate's versions repeatedly).
    reported: std::cell::RefCell<std::collections::BTreeSet<String>>,
    /// Per-run memo of parsed index entries: the resolver asks for one
    /// crate's versions once per parent edge, and every repeated ask is
    /// otherwise a fresh HTTP revalidation (~120 ms each — thousands of
    /// round trips on a real graph).
    memo: std::cell::RefCell<std::collections::BTreeMap<String, Vec<IndexVersion>>>,
}

impl IndexClient {
    /// Creates a client for `config` using `cache_dir` (e.g.
    /// `<store>/index/`).
    pub fn new(cache_dir: PathBuf, config: RegistryConfig) -> Self {
        Self {
            cache_dir,
            config,
            offline: false,
            reported: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            memo: std::cell::RefCell::new(std::collections::BTreeMap::new()),
        }
    }

    /// The registry config.
    pub fn config(&self) -> &RegistryConfig {
        &self.config
    }

    /// Sets offline mode: index entries come from the cache only.
    pub fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
    }

    /// The index cache directory.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Fetches (or serves from cache) every version entry of `name`.
    pub fn versions(&self, name: &str) -> Result<Vec<IndexVersion>, FetchError> {
        if let Some(cached) = self.memo.borrow().get(name) {
            return Ok(cached.clone());
        }
        let versions = self.versions_uncached(name)?;
        self.memo
            .borrow_mut()
            .insert(name.to_owned(), versions.clone());
        Ok(versions)
    }

    /// The uncached fetch path (memoized by [`Self::versions`]).
    fn versions_uncached(&self, name: &str) -> Result<Vec<IndexVersion>, FetchError> {
        let path = index_path_for_name(name);
        let url = format!("{}/{}", self.config.index_url.trim_end_matches('/'), path);
        let cache_file = self.cache_file(name);
        let etag_file = cache_file.with_extension("etag");

        let parse = |text: &str, source: &str| -> Result<Vec<IndexVersion>, FetchError> {
            parse_index(text, name, source)
        };

        if let Some(rest) = url.strip_prefix("file://") {
            // Local indexes are read fresh every time (no ETags).
            let bytes = fetch_url(&url, crate::registry::INDEX_SIZE_LIMIT)?;
            let _ = rest;
            return parse(&String::from_utf8_lossy(&bytes), &url);
        }

        let etag = fs::read_to_string(&etag_file).ok();
        let cached = fs::read_to_string(&cache_file).ok();

        // Offline mode: the cache is the only source.
        if self.offline {
            return match cached {
                Some(cached) => parse(&cached, "index cache"),
                None => Err(FetchError::NotFound(format!(
                    "cached index entry for `{name}` (run `tong lock` online once)"
                ))),
            };
        }

        // Revalidate when a cache exists.
        if cached.is_some() {
            if self.reported.borrow_mut().insert(name.to_owned()) {
                tracing::info!(target: "tong::lock", phase = "index", package = %name);
            }
            let t_fetch = std::time::Instant::now();
            let agent = crate::registry::http_agent();
            let mut request = agent.get(&url);
            if let Some(etag) = &etag {
                request = request.header("If-None-Match", etag);
            }
            match request.call() {
                Ok(mut response) => {
                    // ureq 3.4 surfaces 304 as `Ok` with an empty body
                    // (verified), not as `Err(StatusCode(304))`. Treat
                    // both: a 304 must parse the cached copy and must
                    // never clobber the cache with the empty body.
                    if response.status() == 304 {
                        tracing::debug!(
                            target: "tong::lock",
                            phase = "index.revalidate",
                            package = %name,
                            status = 304,
                            duration_ms = t_fetch.elapsed().as_millis() as u64,
                        );
                        return parse(cached.as_deref().unwrap_or(""), "index cache");
                    }
                    let body = response
                        .body_mut()
                        .with_config()
                        .limit(crate::registry::INDEX_SIZE_LIMIT as u64)
                        .read_to_vec()
                        .map_err(|err| FetchError::Http(err.to_string()))?;
                    let text = String::from_utf8_lossy(&body);
                    tracing::debug!(
                        target: "tong::lock",
                        phase = "index.revalidate",
                        package = %name,
                        status = response.status().as_u16(),
                        bytes = body.len(),
                        duration_ms = t_fetch.elapsed().as_millis() as u64,
                    );
                    let parsed = parse(&text, &url)?;
                    // Cache the fresh copy.
                    if let (Some(dir), Ok(mut file)) =
                        (cache_file.parent(), fs::File::create(&cache_file))
                    {
                        let _ = dir;
                        let _ = io::Write::write_all(&mut file, text.as_bytes());
                    }
                    // New ETag: response.headers() -> "etag".
                    if let Some(new_etag) = response
                        .headers()
                        .get("etag")
                        .and_then(|value| value.to_str().ok())
                    {
                        let _ = fs::write(&etag_file, new_etag);
                    }
                    Ok(parsed)
                }
                Err(ureq::Error::StatusCode(304)) => {
                    // Not modified: parse the cached copy.
                    parse(cached.as_deref().unwrap_or(""), "index cache")
                }
                Err(err) => {
                    // Network failure with a valid cache: use it and warn.
                    if let Some(cached) = cached {
                        eprintln!(
                            "tong: warning: cannot reach {url} ({err}); using the cached index"
                        );
                        parse(&cached, "index cache")
                    } else {
                        Err(FetchError::Http(format!("GET {url}: {err}")))
                    }
                }
            }
        } else {
            // Cold cache: plain GET.
            if self.reported.borrow_mut().insert(name.to_owned()) {
                tracing::info!(target: "tong::lock", phase = "index", package = %name);
            }
            let t_fetch = std::time::Instant::now();
            let bytes = fetch_url(&url, crate::registry::INDEX_SIZE_LIMIT)?;
            let text = String::from_utf8_lossy(&bytes);
            tracing::debug!(
                target: "tong::lock",
                phase = "index.fetch",
                package = %name,
                bytes = bytes.len(),
                duration_ms = t_fetch.elapsed().as_millis() as u64,
            );
            let parsed = parse(&text, &url)?;
            if let Some(parent) = cache_file.parent() {
                let _ = fs::create_dir_all(parent);
                if let Ok(mut file) = fs::File::create(&cache_file) {
                    let _ = io::Write::write_all(&mut file, text.as_bytes());
                }
            }
            Ok(parsed)
        }
    }

    fn cache_file(&self, name: &str) -> PathBuf {
        let path = index_path_for_name(name);
        self.cache_dir.join(path)
    }
}

impl crate::resolve::CrateSource for IndexClient {
    fn versions(&self, name: &str) -> Result<Vec<IndexVersion>, FetchError> {
        IndexClient::versions(self, name)
    }
}

/// The index path for a crate name (current crates.io sparse layout):
/// 1-2 letters keep the flat shard, 3-letter names add the first
/// character as an extra directory (`3/s/syn`), everything else is
/// `{first2}/{next2}/{name}`. Verified against cargo's cached index
/// (2026-08-09): the legacy `/3/<name>` 404s on index.crates.io.
fn index_path_for_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    match lower.len() {
        1 => format!("1/{lower}"),
        2 => format!("2/{lower}"),
        3 => format!("3/{}/{lower}", &lower[..1]),
        _ => format!("{}/{}/{}", &lower[..2], &lower[2..4], lower),
    }
}

/// The URL path component of a crate's index file.
pub fn index_path(_config: &RegistryConfig, name: &str) -> String {
    index_path_for_name(name)
}

/// Parses a sparse index file: one JSON object per line.
fn parse_index(text: &str, name: &str, source: &str) -> Result<Vec<IndexVersion>, FetchError> {
    let mut out = Vec::new();
    for (line_number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let version: IndexVersion = serde_json::from_str(line).map_err(|err| {
            FetchError::BadConfig(format!(
                "malformed index entry for `{name}` in {source} (line {}): {err}",
                line_number + 1
            ))
        })?;
        // Forward-compat: skip unknown schema versions.
        if version.v > 2 {
            continue;
        }
        let mut version = version;
        if let Some(features2) = version.features2.take() {
            for (feature, refs) in features2 {
                version.features.entry(feature).or_default().extend(refs);
            }
        }
        out.push(version);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn index_client() -> (tempfile::TempDir, IndexClient) {
        let dir = tempfile::tempdir().unwrap();
        let config = RegistryConfig {
            index_url: "file:///nonexistent".to_owned(),
            dl: String::new(),
            api: None,
            configured: false,
        };
        let client = IndexClient::new(dir.path().join("index"), config);
        (dir, client)
    }

    fn write_index_file(dir: &Path, name: &str, lines: &str) -> String {
        let path = index_path_for_name(name);
        let full = dir.join("index-root").join(&path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, lines).unwrap();
        format!("file://{}", full.display())
    }

    const INDEX_ENTRY: &str = r#"{"name":"foo","vers":"1.0.0","deps":[{"name":"bar","req":"^1","features":["f"],"optional":false,"default_features":true,"target":null,"kind":"normal","package":null}],"cksum":"abc123","features":{"default":[]},"yanked":false,"v":1}"#;

    #[test]
    fn parses_index_entries_and_skips_new_schema() {
        let (dir, _client) = index_client();
        let url = write_index_file(
            dir.path(),
            "foo",
            &format!(
                "{INDEX_ENTRY}\n{{\"name\":\"foo\",\"vers\":\"2.0.0\",\"deps\":[],\"cksum\":\"x\",\"features\":{{}},\"yanked\":false,\"v\":3}}\n"
            ),
        );
        let config = RegistryConfig {
            index_url: url.trim_end_matches("/3/f/foo").to_owned(),
            dl: String::new(),
            api: None,
            configured: false,
        };
        let client = IndexClient::new(dir.path().join("index"), config);
        let versions = client.versions("foo").unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].vers, Version::new(1, 0, 0));
        assert_eq!(versions[0].deps[0].name, "bar");
        assert_eq!(versions[0].deps[0].req, VersionReq::parse("^1").unwrap());
        assert!(versions[0].deps[0].default_features);
    }

    #[test]
    fn merges_features2_into_features() {
        let (dir, _client) = index_client();
        let line = r#"{"name":"foo","vers":"1.0.0","deps":[],"cksum":"x","features":{"a":["b"]},"features2":{"a":["c"]},"yanked":false,"v":2}"#;
        let url = write_index_file(dir.path(), "foo", line);
        let config = RegistryConfig {
            index_url: url.trim_end_matches("/3/f/foo").to_owned(),
            dl: String::new(),
            api: None,
            configured: false,
        };
        let client = IndexClient::new(dir.path().join("index"), config);
        let versions = client.versions("foo").unwrap();
        let merged = versions[0].features["a"].clone();
        assert!(merged.contains(&"b".to_owned()));
        assert!(merged.contains(&"c".to_owned()));
    }

    #[test]
    fn flags_yanked_versions() {
        let (dir, _client) = index_client();
        let line = r#"{"name":"foo","vers":"1.0.0","deps":[],"cksum":"x","features":{},"yanked":true,"v":1}"#;
        let url = write_index_file(dir.path(), "foo", line);
        let config = RegistryConfig {
            index_url: url.trim_end_matches("/3/f/foo").to_owned(),
            dl: String::new(),
            api: None,
            configured: false,
        };
        let client = IndexClient::new(dir.path().join("index"), config);
        let versions = client.versions("foo").unwrap();
        assert!(versions[0].yanked);
    }

    #[test]
    fn cache_paths_follow_cargo_rules() {
        assert_eq!(index_path_for_name("a"), "1/a");
        assert_eq!(index_path_for_name("ab"), "2/ab");
        // 3-letter crates moved to `3/<first>/<name>` on index.crates.io;
        // the legacy `/3/<name>` 404s (verified 2026-08-09).
        assert_eq!(index_path_for_name("syn"), "3/s/syn");
        assert_eq!(index_path_for_name("cc"), "2/cc");
        assert_eq!(index_path_for_name("serde"), "se/rd/serde");
        assert_eq!(index_path_for_name("serde_derive"), "se/rd/serde_derive");
    }

    /// A minimal HTTP index server for revalidation tests: serves the
    /// entry with an ETag on the first request and 304 on any request
    /// carrying a matching `If-None-Match`.
    struct RevalidateServer {
        addr: std::net::SocketAddr,
    }

    impl RevalidateServer {
        fn start(body: &'static str, etag: &'static str) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut buf = [0u8; 4096];
                    let Ok(n) = stream.read(&mut buf) else {
                        continue;
                    };
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let (status, extra) = if request.contains("If-None-Match") {
                        ("304 Not Modified", "")
                    } else {
                        ("200 OK", &format!("ETag: {etag}\r\n")[..])
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}\r\n",
                        if status.starts_with("304") {
                            0
                        } else {
                            body.len()
                        }
                    );
                    let _ = stream.write_all(head.as_bytes());
                    if status.starts_with("200") {
                        let _ = stream.write_all(body.as_bytes());
                    }
                }
            });
            Self { addr }
        }
    }

    /// ureq 3.4 surfaces 304 as `Ok(status=304)` with an empty body (not
    /// as an error): revalidation must parse the cached copy and never
    /// clobber it with the empty body.
    #[test]
    fn revalidation_serves_cached_copy_on_304() {
        let server = RevalidateServer::start(INDEX_ENTRY, "\"tag-1\"");
        let dir = tempfile::tempdir().unwrap();
        let config = RegistryConfig {
            index_url: format!("http://{}", server.addr),
            dl: String::new(),
            api: None,
            configured: false,
        };
        let client = IndexClient::new(dir.path().join("index"), config);

        // Cold fetch: 200 + body, cached with the ETag.
        let versions = client.versions("foo").unwrap();
        assert_eq!(versions.len(), 1);
        let cache_file = dir.path().join("index/3/f/foo");
        let cached = fs::read_to_string(&cache_file).unwrap();
        assert!(cached.contains("1.0.0"), "{cached}");

        // Warm revalidation: the server answers 304; the cached copy is
        // parsed and left intact.
        let versions = client.versions("foo").unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].vers, Version::new(1, 0, 0));
        assert_eq!(fs::read_to_string(&cache_file).unwrap(), cached);
    }
}
