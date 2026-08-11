# Cargo Import Mode

Without a `Tong.toml`, Tong imports the workspace's `Cargo.toml` and
`Cargo.lock`-equivalent state directly. No `Cargo.toml` is generated,
no Cargo is invoked, and the workspace keeps working with Cargo —
`tong build` and `cargo build` coexist on the same sources
(the `examples/05-voxel-city-cargo` and `06-sdl3-cargo` workspaces are
built and run by both tools, producing identical binaries).

Import happens at analysis time; every lowered unit is an explicit action
with declared inputs. Compatibility is tracked by the synthetic and pinned
external corpora; unsupported behavior must fail explicitly rather than be
silently reinterpreted.

## Supported Cargo features

Covered by focused fixtures in `tong-rust/tests/compat/`; the required
external corpus remains the release-level compatibility gate:

- **Workspace members and path dependencies**, including dependencies
  *outside* the workspace (canonicalized, deduplicated, cycle-checked).
  External crate roots are mounted into the exec root at plan time.
- **Workspace inheritance** (`version.workspace = true`,
  `edition.workspace = true` from `[workspace.package]`); an inheritance
  without a workspace source is a targeted error.
- **Libraries and binaries**, `[lib] name` / `[[bin]]` renames, crate
  types (`rlib`, `cdylib`, `staticlib`, `dylib`), proc macros.
- **Build scripts** (`build.rs`, `build = true/false`), executed as
  explicit actions. The `cargo:` directive dialect is fully parsed —
  both legacy `cargo:key=value` and namespaced `cargo::key=value`:

  - `rustc-cfg`, `rustc-check-cfg`
  - `rustc-env` and the legacy metadata form (unknown `cargo:KEY=VALUE`
    becomes an env pair for dependents)
  - `rustc-link-lib` (with `static:`/`dylib:`/`framework:` kinds),
    `rustc-link-search`, `rustc-flags`, `rustc-link-arg`
    (`-bins`/`-tests`/`-examples`), `rustc-cdylib-link-arg`
  - `rustc-metadata`, `rerun-if-changed`, `rerun-if-env-changed`
  - `warning`, `error`; unknown *namespaced* directives fail the script
    (Cargo errors too), while non-directive stdout lines are ignored

  Directives are propagated with Cargo's semantics: a `rustc-link-lib`
  from `sdl3-sys`'s build script reaches every crate that depends on
  the package.
- **Features**: declared features, default features, and
  Cargo-compatible unification; `--features` / `--no-default-features`
  / `--all-features` on the CLI select per package. Feature resolution
  is a pure, separately tested resolver component.
- **Target-specific dependencies** (`[target.'cfg(...)'.dependencies]`).
- **Tests and benches** (`[[test]]`, `[[bench]]`, and the auto-derived
  lib unit test) — run via `tong test`.
- **Profiles** from `[profile.<name>]` (opt-level, lto, debug,
  overflow-checks, codegen-units, panic, and friends).

## Registry dependencies and `Tong.lock`

Registry dependencies (`axum = "0.8"`, `tokio = "1"`) resolve through
Tong's own lockfile, not `Cargo.lock`:

```sh
tong lock          # resolve versions against the index, write Tong.lock
tong fetch         # download locked crates into the source store
tong build         # fully offline from here on
```

- `tong lock` uses a Cargo-derived resolver (semver-compatible
  activation groups, DFS with backtracking, conflict backjumping) and
  locks the *activated* feature graph — the same shape as `Cargo.lock`.
- `tong fetch` downloads only what the lockfile names and is a no-op
  when everything is stored; `--offline` fails on anything missing.
- `tong update` / `tong update <package>` re-resolve, optionally
  dropping one package's lockfile preference.
- `tong build` / `run` / `test` auto-run `tong lock` and `tong fetch`
  (with a notice) when `Tong.lock` is missing or a locked source
  archive is absent — Cargo generates `Cargo.lock` the same way. For
  reproducible builds, commit `Tong.lock` and run lock/fetch as
  repo-side steps.
- A lockfile that no longer satisfies a manifest requirement fails with
  a targeted diagnostic (`run tong lock`), never a silent re-resolve.

**Normal builds never touch the network.** Network access is confined
to `tong lock`/`tong fetch` (and the toolchain fetcher).

Build scripts and proc macros run as sandboxed actions. Their approved
filesystem, environment, helper-process, and network capabilities are kept in
the checked-in `Tong.permissions.toml` companion file so Cargo continues to
parse the original manifests. Run `tong audit permissions` to generate a
reviewable candidate; normal and CI builds fail rather than prompting when a
new access appears.

## Unsupported Cargo features

Unsupported behavior fails with a targeted diagnostic rather than
silently changing semantics:

- Resolver 1 is not implemented. Older workspaces must opt into resolver 2;
  resolver 2 and 3 are the certification targets.
- Build-affecting manifest/configuration forms not represented in the
  compatibility matrix remain unsupported even when their TOML parses.

If a compatibility gap surprises you, check the corpus under
`tong-rust/tests/compat/` and the roadmap before assuming a bug — but do
report it; explicit failure is the contract.

## Practical notes

- Tong never runs `cargo` (not even `cargo metadata`). Registry/git
  resolution happens against `Tong.lock` + the source store.
- Build scripts and proc macros are explicit actions with their own
  digests; a build-script rerun directive (`rerun-if-changed`) narrows
  the script's declared inputs.
- The first build in a fresh checkout runs `tong lock` + `tong fetch`
  automatically; in Docker, prefer the staged
  `--deps-only` pattern ([Docker Caching](docker.html)) so dependency
  layers bust only on lockfile/toolchain changes.
