//! Sandbox enforcement (PLAN.md section 11).
//!
//! Level 1 is the executor's default clean environment; levels 2-4 add
//! process isolation via platform sandboxes:
//!
//! - L2: clean environment only (no extra enforcement — same as L1 today).
//! - L3: filesystem isolation (read-only inputs, writable only out/tmp).
//! - L4: L3 + network denial.
//!
//! Linux uses bubblewrap (`bwrap`), macOS uses Seatbelt
//! (`sandbox-exec` — removed in recent macOS releases; an actionable error
//! is raised when absent), Windows is a documented no-op beyond the clean
//! environment. Sandboxing is opt-in (`[policy] sandbox`, default `l1`)
//! until certified per platform.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Sandbox enforcement level (PLAN.md section 11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SandboxLevel {
    /// Clean environment only (current executor behavior).
    L1,
    /// Clean environment (no extra enforcement on any platform yet).
    L2,
    /// Filesystem isolation: inputs read-only, only out/tmp writable.
    L3,
    /// L3 plus network denial.
    L4,
}

impl SandboxLevel {
    /// Parses a `Tong.toml` value (`l1`..`l4`).
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "l1" => Some(Self::L1),
            "l2" => Some(Self::L2),
            "l3" => Some(Self::L3),
            "l4" => Some(Self::L4),
            _ => None,
        }
    }
}

/// The sandbox configuration of one execution.
#[derive(Clone, Debug)]
pub struct SandboxSpec {
    /// Enforcement level.
    pub level: SandboxLevel,
    /// Host directories bound read-only (system libraries the captured
    /// toolchain still needs, plus registered toolchain roots).
    pub read_only_binds: Vec<PathBuf>,
    /// Directories the action may write (out/, tmp/).
    pub writable: Vec<PathBuf>,
    /// Deny all network access.
    pub deny_network: bool,
    /// The action's environment (re-applied inside the sandbox).
    pub environment: Vec<(String, String)>,
}

/// A platform sandbox wrapper.
pub trait Sandbox {
    /// Prepends the sandbox invocation to `cmd`. `exec_root` is the
    /// action's working directory (bound read-only; writable subdirs come
    /// from `spec.writable`).
    fn wrap_command(
        &self,
        cmd: &mut Command,
        spec: &SandboxSpec,
        exec_root: &Path,
    ) -> io::Result<()>;
}

/// The sandbox for the host platform: `linux` → bubblewrap, `macos` →
/// Seatbelt, anything else → no-op with a one-time warning.
pub fn sandbox_for(host: &str) -> Box<dyn Sandbox> {
    match host {
        "linux" => Box::new(Bwrap),
        "macos" => Box::new(Seatbelt),
        _ => Box::new(Noop),
    }
}

/// Default read-only system binds for Linux sandboxes (the captured
/// toolchain's runtime closure: loader, libc, SSL certs, locale).
pub fn default_read_only_binds() -> Vec<PathBuf> {
    ["/usr", "/lib", "/lib64", "/etc"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

/// Default readable system paths for macOS Seatbelt sandboxes: system
/// libraries, the dyld shared cache, and the runtime binaries under /bin.
pub fn default_seatbelt_read_paths() -> Vec<PathBuf> {
    ["/usr", "/bin", "/System/Library", "/Library/Apple"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

/// Linux bubblewrap sandbox.
pub struct Bwrap;

impl Sandbox for Bwrap {
    fn wrap_command(
        &self,
        cmd: &mut Command,
        spec: &SandboxSpec,
        exec_root: &Path,
    ) -> io::Result<()> {
        if which("bwrap").is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bwrap is required for sandbox level >= l3 on Linux \
                 (install bubblewrap: apt install bubblewrap / dnf install bubblewrap)",
            ));
        }
        let mut args: Vec<String> = vec![
            "--die-with-parent".to_owned(),
            "--unshare-uts".to_owned(),
            "--unshare-ipc".to_owned(),
            "--unshare-pid".to_owned(),
        ];
        if spec.deny_network {
            args.push("--unshare-net".to_owned());
        }
        // The exec root is bound read-only; writable dirs inside it are
        // re-bound as writable afterwards (bwrap applies binds in order).
        for dir in &spec.writable {
            args.push("--bind".to_owned());
            args.push(dir.to_string_lossy().into_owned());
            args.push(dir.to_string_lossy().into_owned());
        }
        args.push("--tmpfs".to_owned());
        args.push("/tmp".to_owned());
        args.push("--proc".to_owned());
        args.push("/proc".to_owned());
        args.push("--dev".to_owned());
        args.push("/dev".to_owned());
        args.push("--clearenv".to_owned());
        for (key, value) in &spec.environment {
            args.push("--setenv".to_owned());
            args.push(key.clone());
            args.push(value.clone());
        }
        for dir in &spec.read_only_binds {
            args.push("--ro-bind".to_owned());
            args.push(dir.to_string_lossy().into_owned());
            args.push(dir.to_string_lossy().into_owned());
        }
        // The exec root: read-only inputs (writable subdirs already bound).
        args.push("--ro-bind".to_owned());
        args.push(exec_root.to_string_lossy().into_owned());
        args.push(exec_root.to_string_lossy().into_owned());
        args.push("--".to_owned());

        let mut inner: Vec<String> = vec![cmd.get_program().to_string_lossy().into_owned()];
        inner.extend(cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()));
        args.extend(inner);
        let cwd = cmd.get_current_dir().map(|dir| dir.to_path_buf());
        *cmd = Command::new("bwrap");
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        Ok(())
    }
}

/// macOS Seatbelt sandbox (`sandbox-exec`).
///
/// `sandbox-exec` is deprecated and its `deny default` profiles abort at
/// exec time on Sequoia (macOS 15) and later; on such releases the sandbox
/// is reported unavailable and L3/L4 builds fail with an actionable error
/// (the platform is documented as L1/L2-only there, PLAN.md section 11).
pub struct Seatbelt;

impl Sandbox for Seatbelt {
    fn wrap_command(
        &self,
        cmd: &mut Command,
        spec: &SandboxSpec,
        exec_root: &Path,
    ) -> io::Result<()> {
        if !seatbelt_usable() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "sandbox-exec is required for sandbox level >= l3 on macOS and its \
                 deny-default profiles are not usable on this release (Sequoia+); \
                 the sandbox is documented as L1/L2-only there",
            ));
        }
        let mut profile = String::from("(version 1) (deny default) (allow process*)\n");
        let mut allow_read = String::new();
        let mut allow_write = String::new();
        allow_read.push_str(&format!("(subpath \"{}\") ", exec_root.display()));
        for dir in &spec.writable {
            allow_read.push_str(&format!("(subpath \"{}\") ", dir.display()));
            allow_write.push_str(&format!("(subpath \"{}\") ", dir.display()));
        }
        // System paths the sandboxed process needs to load (libraries,
        // dyld cache, runtime binaries) plus the executor's binds
        // (captured toolchain roots).
        for dir in default_seatbelt_read_paths()
            .into_iter()
            .chain(spec.read_only_binds.iter().cloned())
        {
            allow_read.push_str(&format!("(subpath \"{}\") ", dir.display()));
        }
        profile.push_str(&format!("(allow file-read* {allow_read})\n"));
        profile.push_str(&format!("(allow file-write* {allow_write})\n"));
        if spec.deny_network {
            profile.push_str("(deny network*)\n");
        }

        let mut inner: Vec<String> = vec![cmd.get_program().to_string_lossy().into_owned()];
        inner.extend(cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()));
        let cwd = cmd.get_current_dir().map(|dir| dir.to_path_buf());
        *cmd = Command::new("sandbox-exec");
        cmd.arg("-p").arg(profile).arg("--").args(inner);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        Ok(())
    }
}

/// Windows (and unknown hosts): clean environment only; a one-time warning
/// documents the gap (PLAN.md section 11 Windows notes).
pub struct Noop;

impl Sandbox for Noop {
    fn wrap_command(
        &self,
        _cmd: &mut Command,
        spec: &SandboxSpec,
        _exec_root: &Path,
    ) -> io::Result<()> {
        if spec.level >= SandboxLevel::L3 {
            eprintln!(
                "tong: warning: sandbox level {:?} is not enforced on this platform; \
                 only the clean environment applies",
                spec.level
            );
        }
        Ok(())
    }
}

/// Finds an executable on PATH.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Environment map helper: converts `BTreeMap<String, String>` pairs.
pub fn env_pairs(env: &BTreeMap<String, String>) -> Vec<(String, String)> {
    env.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Whether the platform sandbox is present AND usable (tests skip
/// otherwise).
///
/// `sandbox-exec` ships in recent macOS releases but its `deny default`
/// profiles abort at exec time on Sequoia and later (the deprecated
/// Seatbelt API is effectively unusable there); the probe detects that and
/// reports the sandbox as unavailable, matching the per-release status
/// documented on the [`Seatbelt`] type.
pub fn sandbox_available(host: &str) -> bool {
    match host {
        "linux" => which("bwrap").is_some(),
        "macos" => seatbelt_usable(),
        _ => false,
    }
}

/// Probes whether Seatbelt can run a `deny default` profile at all.
fn seatbelt_usable() -> bool {
    let Some(binary) = which("sandbox-exec") else {
        return false;
    };
    let status = std::process::Command::new(binary)
        .args([
            "-p",
            "(version 1) (deny default) (allow process*)",
            "--",
            "/usr/bin/true",
        ])
        .status();
    status.is_ok_and(|status| status.success())
}

/// The exec root: the executor's per-action working directory.
pub fn exec_root_path(exec_base: &Path, action_digest: &str) -> PathBuf {
    exec_base.join(action_digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sandbox_levels() {
        assert_eq!(SandboxLevel::parse("l1"), Some(SandboxLevel::L1));
        assert_eq!(SandboxLevel::parse("l4"), Some(SandboxLevel::L4));
        assert_eq!(SandboxLevel::parse("l5"), None);
        assert_eq!(SandboxLevel::parse("strict"), None);
    }
}
