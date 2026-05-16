# Hermeticity Model

Tong 0.1.0 defines hermeticity at the action boundary:

- Every action has an explicit executable, arguments, environment, inputs,
  outputs, and working directory.
- The action cache key includes the Rust compiler version, profile, host
  platform, command arguments, environment, declared input file contents, and
  output paths.
- Rust actions run with `env_clear`, plus only `LANG`, `LC_ALL`, `TMPDIR`,
  `TMP`, and `TEMP`.
- Source inputs are discovered from package manifests and package files,
  excluding build output directories.
- Path dependency outputs are passed explicitly with `--extern`.
- `build.rs` scripts are compiled and run as separate actions with a minimal
  Cargo-like environment.
- Proc-macro crates are compiled as host dynamic compiler plugins and passed
  explicitly with `--extern`.
- Fetched source dependencies are stored under `target/tong/store/sources`.
- Build outputs live under `target/tong`.

OS-level enforcement is future work:

- Linux: namespaces, read-only bind mounts, seccomp, and network namespaces.
- macOS: sandbox profiles or a dedicated sandbox launcher.
- Windows: Job Objects, restricted tokens, ACL-isolated directories, and
  explicit DLL closure handling.
