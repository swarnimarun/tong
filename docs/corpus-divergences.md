# Corpus divergences (known, documented gaps)

The required corpus tier gates (`corpus-resolve` / `corpus-build`) compare Tong's
resolver graph and builds against `cargo metadata --locked --offline` and Cargo
builds for eleven pinned upstream workspaces. This file records every entry
that does not yet pass, with the observed divergence and its root cause. The
list is the honest status as of this commit; entries are moved to `pass` only
when the underlying Tong behavior is fixed and the gate is green again.

Each entry's `divergence` field in `tests/corpus.toml` names the failing stage
and a one-line reason; the runner's `report.json` records the observed result.

## Required tier

| Entry | Stage | Divergence |
|---|---|---|
| anyhow | resolve | pass |
| hyper | resolve | pass |
| serde | resolve | pass |
| axum | resolve | `cookie`'s implicit `aes-gcm` feature cannot resolve |
| clap | resolve | feature sets of inactive (lock-only) packages miss cargo's default-feature expansion |
| reqwest | resolve | `hyper-util` references the target-table dep `system-configuration`, absent from the host model |
| ripgrep | resolve | feature sets of inactive packages miss cargo's default-feature expansion (`cc`, `shlex`) |
| syn | resolve | duplicate action id `rust:lib:syn@3.0.3:rlib`: the member and a same-name same-version registry `syn` collide |
| tokio | resolve | feature resolution references unknown package `tokio` (workspace self-referencing dev edges) |
| tracing | resolve | manifest parse error: `[profile] strip = true` (Cargo accepts a boolean) |
| wgpu | resolve | `env_logger 0.11.9 is not a registry package`: the pinned git `env_logger` collides with a registry version |

## Detail and root cause

### axum — `cookie`'s implicit `aes-gcm` feature

`cookie`'s `private` feature references the optional dependency `aes-gcm`
(`private = ["aes-gcm", …]`, legacy plain-reference form). `cookie` is an
optional dep of `axum-extra`; when activated, `axum-extra`'s
`cookie-private = ["dep:cookie", "cookie/aes-gcm", …]` requests `aes-gcm` on
the `cookie` edge. The feature resolver requires the `aes-gcm` dependency edge
to exist in the model, but the lock only records it when the optional dep is
feature-activated during lock resolution; the chain of implicit activations
across two crates is not fully closed. Cargo resolves this because its
feature graph is resolved over the full manifest closure.

### clap, ripgrep — default features of inactive packages

`cargo metadata` lists every resolved package's node features with its default
feature expanded (`linux-raw-sys`'s `auxvec`/`elf`/`errno`, `cc`'s `parallel`,
`shlex`'s `default`). Tong's feature map leaves inactive (lock-only) packages
with an empty feature set, so the differential view under-reports. Aligning
this would require applying default features to every locked package in the
view, which risks altering build activation; the comparison normalizer should
be taught the distinction instead.

### reqwest — target-table dependencies absent from the host model

`hyper-util` references `system-configuration` (a `[target.'cfg(macos)'.dependencies]`
entry) in its features. Tong merges target tables host-only, so non-matching
target deps are dropped from the model and their feature references cannot
resolve — cargo keeps all-target deps in its resolve graph for lockfile
completeness (the documented `targets` fixture divergence, now blocking the
required tier).

### syn — duplicate action ids for same-name same-version packages

The `syn` workspace contains the member `syn@3.0.3` and — legitimately, per
cargo's own lock — a registry `syn@3.0.3` (a member's dev-dep on the crates.io
release). The backend's action labels (`rust:lib:syn@3.0.3:rlib`) do not
disambiguate the source, so two packages plan one action id and the scheduler
reports a duplicate. The label must include the source identity.

### tokio — unknown package in feature resolution

The tokio workspace's own members reference `tokio` in dev/feature edges;
during feature resolution the edge resolves to a package identity the model
does not contain (a same-name member/registry collision like syn's). The
resolver must map the edge to the member before the map lookup.

### tracing — `strip = true` profile parse

`inferno`'s manifest declares `[profile.release] strip = true`; Cargo accepts
booleans for `strip`, Tong's profile parser expects the string forms
`none`/`debuginfo`/`symbols` only. The parser needs the boolean arm.

### wgpu — git `env_logger` collides with a registry version

`wgpu` pins `env_logger = { version = "0.11", git = …, rev = "d550741" }`. The
lock records the git source, but the import resolves the edge to the registry
`env_logger 0.11.9` package (`is not a registry package`), so the git checkout
never materializes. The `[patch]`/git-vs-registry name collision needs the same
source-aware disambiguation as the syn case.

## Extended tier

Extended entries (`rustls`, `sqlx`, `bevy`, `cargo`, `rust-analyzer`) record
observed evidence only; they are fetched and reported but never gate.

## Status

As of this commit, 3 of 11 required resolve rows pass (anyhow, hyper, serde)
and the build gate has not yet been run to completion for the remaining
entries. Each divergence above is a distinct, reproducible cargo-parity gap;
none is a formatting or tooling issue. Fixes land in the owning crate
(`tong-rust` model/import/features, `tong-fetch` resolver, `tong-graph`
scheduler) with a synthetic regression before the entry is marked `pass`.
