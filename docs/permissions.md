# Build-script and proc-macro permissions

Build scripts and proc macros are executable dependency code. Tong's target is
not merely to run them with a clean environment, but to make every capability
visible, reviewable, and enforceable.

## Workflow

1. Run `tong audit permissions` for the selected package/target. The audit
   sandbox traces filesystem, environment, child-process, and network access.
2. Review the generated TOML and denied-access diff. Tracing reports any
   platform limitation instead of claiming complete observation.
3. Commit the approved `Tong.permissions.toml` beside the Cargo workspace.
4. Run normal and CI builds. They grant only the committed capabilities and
   fail on new access; CI never prompts.

Audit results are diagnostic only. They are not hermetic, not cache-publishable,
and must not silently update permissions.

## Cargo-import format

```toml
[permissions.my-generator.build_script]
read = ["schemas/**", "vendor/**"]
write = ["$OUT_DIR/**"]
env = ["HOST", "TARGET", "CARGO_PKG_VERSION"]
process = ["//tools:protoc"]
network = false

[permissions.my-generator.proc_macro]
read = ["templates/**"]
write = []
env = ["CARGO_PKG_VERSION"]
process = ["//tools:macro-helper"]
network = false
```

The package key resolves to a source-qualified Cargo package identity. A
proc-macro entry covers both the host macro action and its execution during
rustc expansion; permissions are never inherited from the target crate.

`$OUT_DIR` is automatically writable for the owning build script. Every other
write, read, environment lookup, helper process, or network capability must be
declared. Network-enabled actions are uncacheable and visibly marked in event
logs.

## Security properties

- Permissions are action inputs and change the action digest.
- Filesystem access is restricted to declared read trees and writable output
  trees; undeclared paths fail.
- Helper processes must be declared tools/artifacts, not ambient `PATH` lookups.
- Environment access is allowlisted and credentials are never imported by an
  audit or written to the permission file.
- Network is denied by default. If a platform cannot enforce a requested
  capability, Tong reports the downgrade and refuses a hermetic claim.
- Interactive approval edits the TOML; it is not an implicit runtime grant.

The implementation ladder and platform-specific tracing limitations are
specified in `PLAN.md` §8.6 and §11.
