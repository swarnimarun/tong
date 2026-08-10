//! Fixed-revision Git sources.
//!
//! Two transports:
//!
//! - `gix` (pure Rust, no ambient `git`) for `https://` and `file://`
//!   repositories — hermetic and dependency-free.
//! - The system `git` CLI for SSH-style URLs (`ssh://…` and
//!   `user@host:path`) — gix's SSH support is unreliable, and the ambient
//!   `git` honors the user's SSH keys and agents.
//!
//! `tong lock` resolves a selector (`rev`/`tag`/`branch`, or HEAD) to a
//! 40-hex commit, checks out that exact tree, and captures it into the CAS
//! (the lock records `git+<url>#<commit>` plus the tree digest). Builds and
//! `tong fetch` materialize the locked tree from the CAS and verify the
//! digest — a moving tag or branch never changes what an offline build
//! consumes, and a corrupted stored tree fails verification before any
//! analysis.
//!
//! Repositories are cached bare under `<store>/git/<url-hash>/`; checkouts
//! are transient working trees extracted from the commit's tree objects.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use gix::bstr::ByteSlice;
use tong_core::artifact::TreeDigest;
use tong_core::digest::Hasher;
use tong_store::Cas;

/// A resolved git dependency: the exact commit and its captured source
/// tree.
#[derive(Clone, Debug)]
pub struct ResolvedGit {
    /// 40-hex commit id.
    pub commit: String,
    /// CAS digest of the checked-out source tree.
    pub tree_digest: TreeDigest,
    /// Transient working tree at the commit (caller may remove it once
    /// the tree is captured).
    pub checkout: PathBuf,
}

/// A git package's locked identity: exact commit + captured tree digest.
#[derive(Clone, Debug)]
pub struct LockedGit {
    /// 40-hex commit id.
    pub commit: String,
    /// CAS digest of the source tree captured at lock time.
    pub tree_digest: TreeDigest,
}

/// Git source failure.
#[derive(Debug)]
pub enum GitError {
    /// Transport or repository open failure.
    Repo(String),
    /// The selector could not be resolved to a commit.
    Resolve(String),
    /// The stored tree is missing or fails digest verification.
    Tree(String),
    /// The repository uses submodules, which Tong does not support.
    Submodules(String),
    /// I/O failure.
    Io(io::Error),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Repo(msg) => write!(f, "git: {msg}"),
            Self::Resolve(msg) => write!(f, "git: {msg}"),
            Self::Tree(msg) => write!(f, "git: {msg}"),
            Self::Submodules(msg) => write!(f, "{msg}"),
            Self::Io(err) => write!(f, "git: I/O error: {err}"),
        }
    }
}

impl std::error::Error for GitError {}

impl From<io::Error> for GitError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Which transport serves a repository URL.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    /// Pure-Rust gix (https, file, …).
    Gix,
    /// The ambient `git` CLI (SSH-style URLs, whose authentication gix
    /// cannot reliably drive).
    Cli,
}

/// SSH-style URLs (`ssh://…`, `git@host:path`, `user@host:path`) go
/// through the CLI; everything else uses gix.
fn transport_for(url: &str) -> Transport {
    if url.starts_with("ssh://") || (url.contains('@') && !url.contains("://")) {
        Transport::Cli
    } else {
        Transport::Gix
    }
}

/// The cached bare repository directory for a canonical URL.
fn repo_dir(store: &Path, url: &str) -> PathBuf {
    let hash = Hasher::digest(url.as_bytes()).to_hex();
    store.join("git").join(&hash[..16])
}

fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String, GitError> {
    let mut command = Command::new("git");
    command.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .map_err(|err| GitError::Repo(format!("cannot run `git`: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitError::Repo(format!(
            "`git {}` failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Fetches (or updates) the bare repository for `url` into the store's git
/// cache; returns (repo_dir, transport).
fn fetch_repo(store: &Path, url: &str) -> Result<(PathBuf, Transport), GitError> {
    let transport = transport_for(url);
    let dir = repo_dir(store, url);
    fs::create_dir_all(store.join("git"))?;
    if dir.join("HEAD").is_file() {
        match transport {
            Transport::Cli => {
                // Update refs so a moved tag/branch is visible to
                // `tong update`.
                run_git(
                    &[
                        "-C",
                        dir.to_str().unwrap_or_default(),
                        "fetch",
                        "--quiet",
                        "origin",
                        "+refs/heads/*:refs/heads/*",
                        "+refs/tags/*:refs/tags/*",
                    ],
                    None,
                )?;
            }
            Transport::Gix => {
                // prepare_clone_bare over an existing directory updates
                // it; fall back to a fresh clone when it refuses.
                let result = (|| {
                    let mut prepare = gix::prepare_clone_bare(url, &dir)
                        .map_err(|err| GitError::Repo(err.to_string()))?;
                    prepare
                        .fetch_only(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
                        .map_err(|err| GitError::Repo(err.to_string()))
                })();
                if let Err(err) = result {
                    let _ = fs::remove_dir_all(&dir);
                    let mut prepare = gix::prepare_clone_bare(url, &dir)
                        .map_err(|err| GitError::Repo(err.to_string()))?;
                    prepare
                        .fetch_only(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
                        .map_err(|err| GitError::Repo(err.to_string()))?;
                    let _ = err;
                }
            }
        }
    } else {
        match transport {
            Transport::Cli => {
                run_git(
                    &[
                        "clone",
                        "--quiet",
                        "--bare",
                        url,
                        dir.to_str().unwrap_or_default(),
                    ],
                    None,
                )?;
            }
            Transport::Gix => {
                let mut prepare = gix::prepare_clone_bare(url, &dir)
                    .map_err(|err| GitError::Repo(err.to_string()))?;
                prepare
                    .fetch_only(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
                    .map_err(|err| GitError::Repo(err.to_string()))?;
            }
        }
    }
    Ok((dir, transport))
}

/// Resolves a selector to a 40-hex commit id.
fn resolve_commit(
    transport: Transport,
    repo: &Path,
    url: &str,
    rev: Option<&str>,
    tag: Option<&str>,
    branch: Option<&str>,
) -> Result<String, GitError> {
    let selector = match (rev, tag, branch) {
        (Some(rev), _, _) => rev,
        (None, Some(tag), _) => &format!("refs/tags/{tag}"),
        (None, None, Some(branch)) => &format!("refs/heads/{branch}"),
        (None, None, None) => "HEAD",
    };
    match transport {
        Transport::Cli => {
            let output = run_git(
                &[
                    "-C",
                    repo.to_str().unwrap_or_default(),
                    "rev-parse",
                    "--verify",
                    &format!("{selector}^{{commit}}"),
                ],
                None,
            )?;
            let commit = output.trim().to_owned();
            if !is_hex40(&commit) {
                return Err(GitError::Resolve(format!(
                    "cannot resolve {selector:?} of {url} to a commit"
                )));
            }
            Ok(commit)
        }
        Transport::Gix => {
            let repo = gix::open(repo).map_err(|err| GitError::Repo(err.to_string()))?;
            let id = if let Some(rev) = rev {
                // Full 40-hex ids parse directly; short hash prefixes and
                // refs (e.g. wgpu's `rev = "d550741"`) go through rev-parse
                // so the bare repo resolves the abbreviated id.
                if let Ok(id) = gix::ObjectId::from_hex(rev.as_bytes()) {
                    id
                } else {
                    repo.rev_parse_single(rev)
                        .map(|object| object.detach())
                        .map_err(|_| {
                            GitError::Resolve(format!(
                                "cannot resolve rev {rev:?} of {url}: not a commit id or ref"
                            ))
                        })?
                }
            } else if let Some(tag) = tag {
                repo.find_reference(&format!("refs/tags/{tag}"))
                    .map_err(|_| GitError::Resolve(format!("cannot resolve tag {tag:?} of {url}")))?
                    .peel_to_id()
                    .map_err(|err| GitError::Resolve(err.to_string()))?
                    .detach()
            } else if let Some(branch) = branch {
                repo.find_reference(&format!("refs/heads/{branch}"))
                    .map_err(|_| {
                        GitError::Resolve(format!("cannot resolve branch {branch:?} of {url}"))
                    })?
                    .peel_to_id()
                    .map_err(|err| GitError::Resolve(err.to_string()))?
                    .detach()
            } else {
                repo.head_id()
                    .map_err(|_| GitError::Resolve(format!("{url} has no HEAD")))?
                    .detach()
            };
            repo.find_commit(id).map_err(|err| {
                GitError::Resolve(format!("{url} does not resolve to a commit: {err}"))
            })?;
            Ok(id.to_hex().to_string())
        }
    }
}

fn is_hex40(text: &str) -> bool {
    text.len() == 40 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// Writes a gix tree object's contents to `dest` (recursively). Gitlink
/// (submodule) entries are rejected; `.gitmodules` presence is checked by
/// the caller.
fn write_tree_gix(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    dest: &Path,
) -> Result<(), GitError> {
    let tree = repo
        .find_tree(tree_id)
        .map_err(|err| GitError::Repo(err.to_string()))?;
    fs::create_dir_all(dest)?;
    for entry in tree.iter() {
        let entry = entry.map_err(|err| GitError::Repo(err.to_string()))?;
        let name = entry.filename().to_str_lossy().into_owned();
        let mode = entry.mode();
        let oid = entry.oid().to_owned();
        match mode.kind() {
            gix::objs::tree::EntryKind::Tree => {
                write_tree_gix(repo, oid, &dest.join(&name))?;
            }
            gix::objs::tree::EntryKind::Blob | gix::objs::tree::EntryKind::BlobExecutable => {
                let blob = repo
                    .find_blob(oid)
                    .map_err(|err| GitError::Repo(err.to_string()))?;
                let path = dest.join(&name);
                fs::write(&path, &blob.data)?;
                if mode.kind() == gix::objs::tree::EntryKind::BlobExecutable {
                    set_executable(&path)?;
                }
            }
            gix::objs::tree::EntryKind::Link => {
                // A symlink: write the target path as the file content so
                // the tree is self-contained (no host-specific symlinks).
                let blob = repo
                    .find_blob(oid)
                    .map_err(|err| GitError::Repo(err.to_string()))?;
                fs::write(dest.join(&name), &blob.data)?;
            }
            gix::objs::tree::EntryKind::Commit => {
                return Err(GitError::Submodules(
                    "git dependency uses submodules, which Tong does not support".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Checks out `commit` into `dest` (creating it) and rejects submodules.
fn checkout_commit(
    transport: Transport,
    repo: &Path,
    url: &str,
    commit: &str,
    dest: &Path,
) -> Result<(), GitError> {
    let _ = fs::remove_dir_all(dest);
    fs::create_dir_all(dest)?;
    match transport {
        Transport::Cli => {
            // `git archive` streams the exact tree (modes preserved);
            // `tar -x` extracts it.
            let mut child = Command::new("git")
                .args([
                    "-C",
                    repo.to_str().unwrap_or_default(),
                    "archive",
                    "--format=tar",
                    commit,
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .map_err(|err| GitError::Repo(format!("cannot run `git archive`: {err}")))?;
            let tar = Command::new("tar")
                .args(["-xf", "-", "-C", dest.to_str().unwrap_or_default()])
                .stdin(std::process::Stdio::from(child.stdout.take().unwrap()))
                .status()
                .map_err(|err| GitError::Repo(format!("cannot run `tar`: {err}")))?;
            let git_status = child
                .wait()
                .map_err(|err| GitError::Repo(format!("`git archive` failed: {err}")))?;
            if !git_status.success() || !tar.success() {
                return Err(GitError::Repo(format!("failed to extract {url}#{commit}")));
            }
        }
        Transport::Gix => {
            let repo = gix::open(repo).map_err(|err| GitError::Repo(err.to_string()))?;
            let id = gix::ObjectId::from_hex(commit.as_bytes())
                .map_err(|err| GitError::Resolve(err.to_string()))?;
            let commit_obj = repo
                .find_commit(id)
                .map_err(|err| GitError::Repo(err.to_string()))?;
            let tree_id = commit_obj
                .tree_id()
                .map_err(|err| GitError::Repo(err.to_string()))?;
            write_tree_gix(&repo, tree_id.into(), dest)?;
        }
    }
    if dest.join(".gitmodules").is_file() {
        return Err(GitError::Submodules(format!(
            "git dependency #{commit} uses submodules, which Tong does not support"
        )));
    }
    Ok(())
}

/// Resolves a git selector to a locked commit and captures its source
/// tree into the CAS.
///
/// When `prefer_locked` and `locked` are given, the locked commit and its
/// stored tree are reused verbatim — a moving tag/branch never changes the
/// lock (callers pass `prefer_locked: false` for `tong update <package>`).
#[allow(clippy::too_many_arguments)]
pub fn resolve_and_capture(
    store: &Path,
    cas: &Cas,
    url: &str,
    rev: Option<&str>,
    tag: Option<&str>,
    branch: Option<&str>,
    prefer_locked: bool,
    locked: Option<&LockedGit>,
) -> Result<ResolvedGit, GitError> {
    if prefer_locked && let Some(locked) = locked {
        // The locked commit's tree is already in the CAS: reuse it
        // without any network access. The checkout (existing or
        // freshly materialized) must verify against the locked tree
        // digest — a corrupted stored tree fails here.
        let checkout = checkout_dir(store, url, &locked.commit);
        if !checkout.is_dir() {
            if cas.get_tree(locked.tree_digest)?.is_none() {
                return Err(GitError::Tree(format!(
                    "stored tree for {url}#{} is missing; run `tong lock`",
                    locked.commit
                )));
            }
            cas.materialize(locked.tree_digest, &checkout)?;
        }
        let captured = cas.capture_dir(&checkout)?;
        if captured != locked.tree_digest {
            let _ = fs::remove_dir_all(&checkout);
            return Err(GitError::Tree(format!(
                "stored tree for {url}#{} failed verification; run `tong lock` \
                     to re-resolve",
                locked.commit
            )));
        }
        return Ok(ResolvedGit {
            commit: locked.commit.clone(),
            tree_digest: locked.tree_digest,
            checkout,
        });
    }

    let (repo, transport) = fetch_repo(store, url)?;
    let commit = resolve_commit(transport, &repo, url, rev, tag, branch)?;
    // A locked commit that disagrees with a freshly resolved moving
    // selector is rejected: the lock must not silently move.
    if let Some(locked) = locked
        && locked.commit != commit
    {
        return Err(GitError::Resolve(format!(
            "{url} ({}) now points at {commit}, but Tong.lock has {}; \
                 run `tong update <package>` to accept the new commit",
            tag.map(|t| format!("tag {t}"))
                .or_else(|| branch.map(|b| format!("branch {b}")))
                .unwrap_or_else(|| "HEAD".to_owned()),
            locked.commit
        )));
    }
    let checkout = checkout_dir(store, url, &commit);
    if !checkout.is_dir() {
        checkout_commit(transport, &repo, url, &commit, &checkout)?;
    }
    let tree_digest = cas.capture_dir(&checkout)?;
    Ok(ResolvedGit {
        commit,
        tree_digest,
        checkout,
    })
}

/// The checkout directory for a locked (url, commit).
fn checkout_dir(store: &Path, url: &str, commit: &str) -> PathBuf {
    let hash = Hasher::digest(url.as_bytes()).to_hex();
    store
        .join("git")
        .join(&hash[..16])
        .join("checkouts")
        .join(commit)
}

/// Materializes a locked git tree from the CAS into a checkout directory
/// and verifies its digest. Fails before analysis when the stored tree is
/// missing or corrupted.
pub fn materialize_tree(
    cas: &Cas,
    tree_digest: TreeDigest,
    store: &Path,
    url: &str,
    commit: &str,
) -> Result<PathBuf, GitError> {
    let checkout = checkout_dir(store, url, commit);
    if !checkout.is_dir() {
        if cas.get_tree(tree_digest)?.is_none() {
            return Err(GitError::Tree(format!(
                "stored tree for {url}#{commit} is missing; run `tong lock`"
            )));
        }
        cas.materialize(tree_digest, &checkout)?;
    }
    // Verify: the materialized tree must capture to the locked digest.
    let captured = cas.capture_dir(&checkout)?;
    if captured != tree_digest {
        let _ = fs::remove_dir_all(&checkout);
        return Err(GitError::Tree(format!(
            "stored tree for {url}#{commit} failed verification (expected \
             {}, captured {}); run `tong lock` to re-resolve",
            tree_digest.digest(),
            captured.digest()
        )));
    }
    Ok(checkout)
}
