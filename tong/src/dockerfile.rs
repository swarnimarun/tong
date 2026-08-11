//! `tong dockerfile`: emits a layer-cache-friendly `Dockerfile` and
//! `.dockerignore` (docs/docker-caching.md).
//!
//! The generated stages mirror the docker layer split:
//!
//! 1. toolchain — busts only when the tong version pin changes;
//! 2. deps — fetch + `tong build --deps-only`; busts only when manifests,
//!    `Tong.lock`, or the toolchain change;
//! 3. app — `COPY . .` + full build; dep actions are cache hits;
//! 4. runtime — workspace binaries only.
//!
//! Workspace-member manifests are staged at their exact relative paths so
//! planning succeeds on a manifests-only tree (design (b) in
//! docs/docker-caching.md Feature 1): the captured member trees are never
//! executed in the deps stage, so their content is irrelevant. Path
//! dependencies are copied in full (their sources are executed); registry
//! and git checkouts live in the store and arrive via `tong fetch`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::driver::{self, BuildError};

/// `tong dockerfile` command-line options.
#[derive(Clone, Debug)]
pub struct Options {
    /// Profile baked into the generated commands and artifact paths.
    pub profile: String,
    /// Toolchain-stage base image; required unless `[toolchain.rust]`
    /// version is pinned in `Tong.toml` (default `rust:<version>`).
    pub base: Option<String>,
    /// Runtime-stage base image.
    pub runtime_base: String,
    /// Directory to write `Dockerfile` and `.dockerignore` into.
    pub output: PathBuf,
}

/// The generated `Dockerfile` and `.dockerignore` contents.
#[derive(Debug)]
pub struct Generated {
    pub dockerfile: String,
    pub dockerignore: String,
}

/// Generates the Dockerfile + .dockerignore for the workspace at `root`.
pub fn generate(root: &Path, options: &Options) -> Result<Generated, BuildError> {
    let manifest = driver::load_manifest(root)?;
    let model = driver::load_model_unlocked(root, manifest.as_ref())?;
    let root_canon = fs::canonicalize(root)
        .map_err(|err| BuildError::Io(io_err(root, "workspace root", err)))?;
    let store = driver::store_dir(root, manifest.as_ref())?;
    let store_canon = fs::canonicalize(&store).ok();

    // The base image: the pinned [toolchain.rust] version, or --base.
    let base = match &options.base {
        Some(base) => base.clone(),
        None => match manifest
            .as_ref()
            .and_then(|manifest| manifest.toolchain.rust.version.clone())
        {
            Some(version) => format!("rust:{version}"),
            None => {
                return Err(BuildError::Dockerfile(
                    "--base <image> is required: this workspace does not pin \
                     a [toolchain.rust] version in Tong.toml"
                        .to_owned(),
                ));
            }
        },
    };

    let members: BTreeSet<&str> = model.members.iter().map(|id| id.name.as_str()).collect();

    // Member-relative paths (for manifest staging) and path-dependency
    // directories (copied in full). Registry/git checkouts live under the
    // store and must NOT be copied — `tong fetch` provides them.
    let mut member_dirs: Vec<PathBuf> = Vec::new();
    let mut path_deps: Vec<(String, PathBuf)> = Vec::new();
    for pkg in &model.packages {
        let rel = match pkg.dir.strip_prefix(&root_canon) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.to_path_buf(),
            _ => {
                // The package dir is outside the build context (e.g. a
                // path dep at ../other). Docker COPY cannot reach it;
                // warn and skip.
                eprintln!(
                    "tong: warning: package `{}` at {} is outside the build \
                     context and will not be staged; copy it into the context \
                     or vendor it",
                    pkg.name,
                    pkg.dir.display()
                );
                continue;
            }
        };
        if members.contains(pkg.name.as_str()) {
            member_dirs.push(rel);
        } else if store_canon
            .as_ref()
            .is_none_or(|store| !pkg.dir.starts_with(store))
        {
            // A non-member package whose sources do not come from the
            // source store: a path dependency — its sources are needed to
            // plan and execute its actions in the deps stage.
            path_deps.push((pkg.name.clone(), rel));
        }
    }
    member_dirs.sort();
    path_deps.sort();

    // Workspace binaries for the runtime stage (members only; dependency
    // crates rarely ship binaries and must not leak into the image).
    let mut bins: Vec<String> = Vec::new();
    for pkg in &model.packages {
        if !members.contains(pkg.name.as_str()) {
            continue;
        }
        for bin in &pkg.bins {
            bins.push(bin.name.clone());
        }
    }
    bins.sort();
    bins.dedup();

    let native = manifest.is_some();
    let cargo_manifest = root.join("Cargo.toml");
    let lockfile = root.join("Tong.lock");
    let cargo_lock = root.join("Cargo.lock");
    let has_lock = lockfile.is_file();

    let mut lines: Vec<String> = Vec::new();
    lines.push("# syntax=docker/dockerfile:1".to_owned());
    lines.push("# Generated by `tong dockerfile` — do not edit, regenerate.".to_owned());
    lines.push(format!("# Profile: {}", options.profile));
    lines.push(String::new());

    // Stage 0: tong toolchain.
    lines.push(
        "# Stage 0: tong toolchain (busts only when the tong version pin changes).".to_owned(),
    );
    lines.push(format!("FROM {base} AS toolchain"));
    lines.push(format!(
        "RUN cargo install tong --version {} --locked",
        env!("CARGO_PKG_VERSION")
    ));
    lines.push(String::new());

    // Stage 1: fetch + build deps.
    lines
        .push("# Stage 1: fetch + build deps. Busts only when manifests, Tong.lock, or".to_owned());
    lines.push("# the toolchain change — never on source edits.".to_owned());
    lines.push("FROM toolchain AS deps".to_owned());
    lines.push("WORKDIR /app".to_owned());
    if native {
        lines.push("COPY Tong.toml ./".to_owned());
    } else {
        // The workspace manifest is always needed; Cargo.lock is copied
        // for workspace consistency (tong does not read it).
        if cargo_lock.is_file() {
            lines.push("COPY Cargo.toml Cargo.lock ./".to_owned());
        } else {
            lines.push("COPY Cargo.toml ./".to_owned());
        }
        if cargo_manifest.is_file() && lockfile.is_file() {
            lines.push("COPY Tong.lock ./".to_owned());
        }
        // Member manifests at their exact relative paths (design (b)):
        // planning captures the manifest-only trees and never executes
        // them in this stage.
        for dir in &member_dirs {
            let rel = dir.to_string_lossy();
            lines.push(format!("COPY {rel}/Cargo.toml {rel}/Cargo.toml"));
        }
        // Path dependencies: full sources (their actions execute here).
        for (name, dir) in &path_deps {
            let rel = dir.to_string_lossy();
            lines.push(format!(
                "# Path dependency `{name}` (source edits bust this layer)."
            ));
            lines.push(format!("COPY {rel} {rel}"));
        }
    }
    if has_lock {
        lines.push("RUN tong fetch".to_owned());
    }
    lines.push(format!(
        "RUN tong build --deps-only --profile {}",
        options.profile
    ));
    lines.push(String::new());

    // Stage 2: app.
    lines.push(
        "# Stage 2: app. Busts on every source change; dep actions are cache hits.".to_owned(),
    );
    lines.push("FROM deps AS app".to_owned());
    lines.push("COPY . .".to_owned());
    lines.push(format!("RUN tong build --profile {}", options.profile));
    lines.push(String::new());

    // Stage 3: runtime (only when the workspace has binaries).
    if bins.is_empty() {
        lines.push("# No workspace binaries; runtime stage omitted.".to_owned());
    } else {
        lines.push("# Stage 3: runtime.".to_owned());
        lines.push(format!("FROM {} AS runtime", options.runtime_base));
        for bin in &bins {
            lines.push(format!(
                "COPY --from=app /app/.tong/out/{}/{bin}/{bin} /usr/local/bin/{bin}",
                options.profile
            ));
        }
        lines.push(format!("ENTRYPOINT [\"/usr/local/bin/{}\"]", bins[0]));
    }
    lines.push(String::new());

    Ok(Generated {
        dockerfile: lines.join("\n"),
        dockerignore: ".git/\n.tong/\ntarget/\n**/target/\n".to_owned(),
    })
}

fn io_err(root: &Path, what: &str, err: std::io::Error) -> std::io::Error {
    std::io::Error::new(err.kind(), format!("{}: {}: {err}", root.display(), what))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Writes a file, creating parent directories.
    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// A Cargo workspace: members `app` (binary) and `libc` (library), a
    /// path dependency `vendor/depc` (also a binary, which must NOT be
    /// copied to the runtime stage), and a committed Cargo.lock.
    fn cargo_workspace(dir: &Path) {
        write(
            &dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"libc\"]\nresolver = \"2\"\n",
        );
        write(&dir.join("Cargo.lock"), "# fixture lockfile\n");
        write(
            &dir.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndepc = { path = \"../vendor/depc\" }\n",
        );
        write(&dir.join("app/src/main.rs"), "fn main() {}\n");
        write(
            &dir.join("libc/Cargo.toml"),
            "[package]\nname = \"libc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(&dir.join("libc/src/lib.rs"), "pub fn f() {}\n");
        write(
            &dir.join("vendor/depc/Cargo.toml"),
            "[package]\nname = \"depc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(&dir.join("vendor/depc/src/lib.rs"), "pub fn g() {}\n");
        write(&dir.join("vendor/depc/src/main.rs"), "fn main() {}\n");
    }

    #[test]
    fn cargo_workspace_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        cargo_workspace(tmp.path());
        let generated = generate(
            tmp.path(),
            &Options {
                profile: "dev".to_owned(),
                base: Some("rust:1.97-bookworm".to_owned()),
                runtime_base: "debian:bookworm-slim".to_owned(),
                output: PathBuf::new(),
            },
        )
        .expect("generation succeeds");
        let text = generated.dockerfile;
        // Toolchain stage.
        assert!(
            text.contains("FROM rust:1.97-bookworm AS toolchain"),
            "{text}"
        );
        assert!(text.contains(&format!(
            "RUN cargo install tong --version {} --locked",
            env!("CARGO_PKG_VERSION")
        )));
        // Deps stage: manifests staged at real paths, path dep in full.
        assert!(text.contains("COPY Cargo.toml Cargo.lock ./"), "{text}");
        assert!(
            text.contains("COPY app/Cargo.toml app/Cargo.toml"),
            "{text}"
        );
        assert!(
            text.contains("COPY libc/Cargo.toml libc/Cargo.toml"),
            "{text}"
        );
        assert!(text.contains("COPY vendor/depc vendor/depc"), "{text}");
        assert!(
            text.contains("RUN tong build --deps-only --profile dev"),
            "{text}"
        );
        // No Tong.lock in the fixture: no fetch line, no lock COPY.
        assert!(!text.contains("COPY Tong.lock"), "{text}");
        assert!(!text.contains("RUN tong fetch"), "{text}");
        // App + runtime stages; the member binary, not the path dep's.
        assert!(text.contains("FROM deps AS app"), "{text}");
        assert!(text.contains("COPY . ."), "{text}");
        assert!(
            text.contains("FROM debian:bookworm-slim AS runtime"),
            "{text}"
        );
        assert!(text.contains("COPY --from=app /app/.tong/out/dev/app/app /usr/local/bin/app"));
        assert!(!text.contains("depc/depc"), "{text}");
        assert!(
            text.contains("ENTRYPOINT [\"/usr/local/bin/app\"]"),
            "{text}"
        );
        // .dockerignore: the local dev store must never enter the context.
        assert_eq!(
            generated.dockerignore,
            ".git/\n.tong/\ntarget/\n**/target/\n"
        );
    }

    #[test]
    fn cargo_workspace_with_lockfile_emits_fetch() {
        let tmp = tempfile::tempdir().unwrap();
        cargo_workspace(tmp.path());
        write(&tmp.path().join("Tong.lock"), "# fixture\n");
        let generated = generate(
            tmp.path(),
            &Options {
                profile: "release".to_owned(),
                base: Some("rust:1.97-bookworm".to_owned()),
                runtime_base: "debian:bookworm-slim".to_owned(),
                output: PathBuf::new(),
            },
        )
        .expect("generation succeeds");
        let text = generated.dockerfile;
        assert!(text.contains("COPY Tong.lock ./"), "{text}");
        assert!(text.contains("RUN tong fetch"), "{text}");
        assert!(
            text.contains("RUN tong build --deps-only --profile release"),
            "{text}"
        );
    }

    #[test]
    fn native_manifest_pins_base_and_skips_cargo_lines() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("Tong.toml"),
            "schema = 1\n\n[toolchain.rust]\nkind = \"dist\"\nversion = \"1.90.0\"\n\n[target.app]\nrule = \"rust_binary\"\n",
        );
        let generated = generate(
            tmp.path(),
            &Options {
                profile: "dev".to_owned(),
                base: None,
                runtime_base: "debian:bookworm-slim".to_owned(),
                output: PathBuf::new(),
            },
        )
        .expect("pinned version supplies the base image");
        let text = generated.dockerfile;
        assert!(text.contains("FROM rust:1.90.0 AS toolchain"), "{text}");
        assert!(text.contains("COPY Tong.toml ./"), "{text}");
        assert!(!text.contains("Cargo.toml"), "{text}");
        assert!(!text.contains("RUN tong fetch"), "{text}");
        assert!(text.contains("COPY --from=app /app/.tong/out/dev/app/app /usr/local/bin/app"));
    }

    #[test]
    fn unpinned_toolchain_requires_base() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("Tong.toml"),
            "schema = 1\n\n[target.app]\nrule = \"rust_binary\"\n",
        );
        let err = generate(
            tmp.path(),
            &Options {
                profile: "dev".to_owned(),
                base: None,
                runtime_base: "debian:bookworm-slim".to_owned(),
                output: PathBuf::new(),
            },
        )
        .expect_err("--base required without a pinned version");
        match err {
            BuildError::Dockerfile(message) => assert!(message.contains("--base"), "{message}"),
            other => panic!("expected Dockerfile error, got {other:?}"),
        }
    }
}
