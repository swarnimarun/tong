# Hermeticity Model

Tong defines hermeticity at the action boundary.

An action is the smallest cacheable unit. Every language backend should lower its work into actions instead of invoking a package manager or compiler driver with ambient state.

## Action Fields

Each action has:

- `id`
- `mnemonic`
- executable path
- arguments
- environment
- declared inputs
- declared outputs
- working directory
- extra key material

The cache key currently includes all of those fields plus the content hash of every declared input.

## Current Guarantees

The MVP guarantees:

- Rust compile actions use a resolved toolchain `rustc`, not the rustup shim.
- Rust compile actions run with `env_clear`.
- The only default environment variables are `LANG`, `LC_ALL`, `TMPDIR`, `TMP`, and `TEMP`.
- The rustc verbose version is included in every Rust action cache key.
- The build profile and host platform are included in every Rust action cache key.
- Package manifests and package files, excluding build output directories, are declared inputs.
- Path dependency outputs are explicit `--extern` inputs.
- Source dependencies from git, tar, `.crate`, and zip are materialized under `target/tong/store/sources`.
- Archive source dependencies can be checked against a declared `sha256`.
- Simple `build.rs` scripts are compiled and run as separate actions.
- Build script runs receive a minimal Cargo-like environment and write generated files under `OUT_DIR`.
- Build script stdout is captured and parsed into cfg, env, and link arguments for later Rust actions.
- Proc-macro crates are compiled as explicit host artifacts and passed with `--extern`.
- Outputs are placed under `target/tong`.

## Current Non-Guarantees

The MVP does not yet guarantee:

- OS-level filesystem sandboxing.
- Network blocking enforced by the kernel.
- Complete Rust module input discovery outside `src/**/*.rs`.
- Complete `build.rs` compatibility, including build-dependencies and strict undeclared-input detection.
- Native dependency isolation.
- Stable cryptographic hashing.
- Cross-machine cache compatibility.
- Complete Cargo feature unification across multiple dependency paths.

Those are explicit roadmap items, not accidental omissions.

## Future Store Model

Fetched and built dependencies should live in a content-addressed store:

```text
target/tong/store/
  sha256-<hash>-<name>-<version>/
    include/
    lib/
    bin/
```

Store keys should include:

- source hash
- build recipe
- target platform
- host platform
- toolchain identity
- declared environment
- declared dependencies

## Backend Rule

Every future language backend must obey the same contract:

- no ambient dependency discovery;
- no implicit environment dependence;
- no undeclared output writes;
- no network except fixed-output fetch actions;
- all compiler/tool versions included in action keys;
- all dependency artifacts passed explicitly.
