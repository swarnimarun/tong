# Manifest Reference

`Tong.toml` is the canonical build description (native mode). It is
strict TOML: unknown fields are rejected with a parse error. The file is
optional — without it, Tong imports `Cargo.toml` workspaces instead
([Cargo Import Mode](cargo-import.html)).

A minimal workspace:

```toml
[workspace]
name = "hello"

[toolchain.rust]
kind = "system"

[target.hello]
rule = "rust_binary"
crate_root = "src/main.rs"
edition = "2021"
```

## `[workspace]`

| Field | Type | Description |
|---|---|---|
| `name` | string | Workspace display name |

## `[toolchain.rust]`

| Field | Type | Default | Description |
|---|---|---|---|
| `kind` | string | `"system"` | `"system"` — capture the local rustc + sysroot (non-portable, fingerprinted); `"dist"` — use a pinned downloaded toolchain |
| `version` | string | — | Required for `kind = "dist"` (e.g. `"1.90.0"`); fetch with `tong toolchain fetch rust --version <ver>` |

## `[profile.<name>]`

Named build profiles; `dev` and `release` exist by default with Cargo's
defaults. Profile fields map directly to rustc flags.

| Field | Type | Description |
|---|---|---|
| `opt_level` | `0`–`3`, `"s"`, `"z"` | Optimization level |
| `debug` | bool | Debug info |
| `lto` | bool, `"thin"`, `"fat"` | Link-time optimization |
| `panic` | `"unwind"`, `"abort"` | Panic strategy |
| `codegen_units` | int | Codegen units |
| `overflow_checks` | bool | Overflow checks |
| `debug_assertions` | bool | Debug assertions |
| `strip` | `"none"`, `"debuginfo"`, `"symbols"` | Strip policy |
| `rpath` | bool | Pass rpath to the linker |

```toml
[profile.release]
opt_level = 3
lto = "thin"
panic = "abort"
codegen_units = 16
overflow_checks = false
```

## `[target.<name>]`

Named targets. `rule` selects the backend rule; the other fields are
shared across rules.

| Field | Type | Description |
|---|---|---|
| `rule` | string | `rust_library`, `rust_binary`, `rust_proc_macro`, `rust_test`, `cc_import` |
| `crate_root` | path | Crate root, relative to the workspace root; defaults to `src/lib.rs` (library), `src/main.rs` (binary), `tests/<name>.rs` (test) |
| `edition` | string | `2015`, `2018`, `2021`, `2024` (default `2021`) |
| `deps` | `[":a", …]` | Dependency labels (`:name` form) |
| `dev_deps` | `[":c", …]` | Dev-dependencies — used by `rust_test` targets |
| `rustflags` | `[string]` | Extra rustc flags |
| `env` | map | Per-target environment variables |
| `features` | `[string]` | Features to activate on this target |
| `default_features` | bool | Whether the target's default feature is enabled |
| `build_script` | path | Build-script source, relative to the workspace root |
| `crate_types` | `["rlib", "cdylib", "staticlib", "dylib"]` | Crate types for a library target |
| `proc_macro` | bool | Compile as a proc macro |
| `shared` | path | `cc_import`: the shared library to import |
| `link_name` | string | `cc_import`: the `-l` link name (default derived from the file name, e.g. `libSDL3.0.dylib` → `SDL3.0`) |

Cargo-style auto-detection applies: a `rust_binary` with `src/lib.rs`
present also gets a library target, mirroring Cargo's lib+bin packages.

### Rules

**`rust_library`** — an rlib (or the listed `crate_types`).

```toml
[target.codec]
rule = "rust_library"
crate_root = "crates/codec/src/lib.rs"
deps = [":native_codec"]
```

**`rust_binary`** — an executable. Defaults `crate_root` to
`src/main.rs`; auto-detects `src/lib.rs`.

```toml
[target.application]
rule = "rust_binary"
crate_root = "crates/application/src/main.rs"
deps = [":codec"]
```

**`rust_proc_macro`** — a proc-macro crate (`proc_macro = true`
equivalent; the rule name implies it).

**`rust_test`** — an integration test with libtest harness: `deps`
plus `dev_deps`, default path `tests/<name>.rs`.

```toml
[target.integration]
rule = "rust_test"
crate_root = "tests/integration.rs"
deps = [":core"]
dev_deps = [":fixtures"]
```

**`cc_import`** — import a prebuilt native shared library (no rule
fields beyond `shared`/`link_name`). The library is captured into the
content-addressed store at plan time, dependent crates link with
`-L native -l <link_name>`, and the runtime library is shipped next to
the final artifact.

```toml
[target.sdl3]
rule = "cc_import"
shared = "/opt/homebrew/opt/sdl3/lib/libSDL3.0.dylib"
link_name = "SDL3.0"
```

`shared` may be an absolute host path (system-captured, non-portable)
or a workspace-relative path.

Unknown rules are ignored with a warning (they are backend concerns —
the graph layer only models the file), so future rules fail loudly at
the backend, not silently.

## `[store]`

Store policy for the workspace.

| Field | Type | Description |
|---|---|---|
| `dir` | path | Relocate the content-addressed store (shared-store mode). Relative to the workspace root; must not contain `..` |
| `retention` | string | Auto-GC age floor for unmarked objects (`7d`, `30d`) |
| `max_size` | string | Store size budget (`10G`, `500M`) |

Environment overrides: `TONG_STORE_DIR`, `TONG_STORE_RETENTION`,
`TONG_STORE_MAX_SIZE`.

```toml
[store]
dir = "../shared-store"
retention = "30d"
max_size = "20G"
```

## `[registry]`

| Field | Type | Description |
|---|---|---|
| `index` | string | Registry index URL: `sparse+https://…`, `https://…` (git), or `file://…` (local). Default: crates.io sparse index |

`TONG_REGISTRY_INDEX` wins over this field. Only relevant in Cargo
import mode (native mode is offline path-only).

```toml
[registry]
index = "sparse+https://index.crates.io/"
```

## `[policy]`

Execution policy. Sandboxing is **opt-in** until certified per
platform.

| Field | Type | Description |
|---|---|---|
| `sandbox` | string | Enforcement level: `l1` (clean environment — default), `l2`, `l3` (filesystem isolation), `l4` (l3 + network denial) |

```toml
[policy]
sandbox = "l3"
```

See [Hermeticity Model](hermeticity.html) for what each level enforces
per platform.

## Full example

```toml
[workspace]
name = "sdl3-demo"

[toolchain.rust]
kind = "system"

[profile.dev]
opt_level = 0
debug = true

[store]
retention = "7d"
max_size = "10G"

[target.sdl3]
rule = "cc_import"
shared = "/opt/homebrew/opt/sdl3/lib/libSDL3.0.dylib"
link_name = "SDL3.0"

[target.sdl3_sys]
rule = "rust_library"
crate_root = "crates/sdl3-sys/src/lib.rs"
deps = [":sdl3"]

[target.sdl3_demo]
rule = "rust_binary"
crate_root = "crates/sdl3-demo/src/main.rs"
deps = [":sdl3_sys"]
```
