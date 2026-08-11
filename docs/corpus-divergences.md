# Corpus compatibility evidence

The required corpus tier compares Tong's resolved graph with
`cargo metadata --locked --offline` and then runs each entry's declared Tong
build gate. The corpus is pinned in `tests/corpus.toml`; the runner writes the
machine-readable evidence to `$TONG_CORPUS_DIR/report.json`.

This document is a snapshot, not a second source of truth. Regenerate it only
from a fresh `corpus-resolve` and `corpus-build` run. A row is `pass` only when
the generated report says so.

## Required tier

Fresh evidence from 2026-08-12 01:12:34 IST on `macos-aarch64`:

| Entry | Resolver | Resolver time | Offline build | Build time |
|---|---:|---:|---:|---:|
| anyhow | pass | 1.160 s | pass | 25.956 s |
| axum | pass | 24.302 s | pass | 575.945 s |
| clap | pass | 6.504 s | pass | 62.914 s |
| hyper | pass | 2.866 s | pass | 15.469 s |
| reqwest | pass | 9.467 s | pass | 12.755 s |
| ripgrep | pass | 2.517 s | pass | 27.442 s |
| serde | pass | 1.038 s | pass | 1.723 s |
| syn | pass | 8.912 s | pass | 3.145 s |
| tokio | pass | 7.553 s | pass | 20.236 s |
| tracing | pass | 10.053 s | pass | 234.333 s |
| wgpu | pass | 28.380 s | pass | 90.452 s |

Required result: **11/11 resolver rows and 11/11 offline build rows pass** on
the recorded platform.

Several upstream workspaces cannot use a literal `--all-targets
--all-features` stable-toolchain gate. The pinned gate instead exercises the
broadest stable workflow that the upstream project supports:

- ripgrep excludes benchmark targets because its benches require nightly;
- syn checks its library with `full`, `visit`, `visit-mut`, `fold`, and
  `extra-traits`, because its full target set requires rustc-private crates;
- tokio uses the full default-feature workspace because some optional targets
  additionally require the upstream-only `tokio_unstable` cfg.

These are corpus-definition constraints, not Tong divergences. Each command is
recorded in `tests/corpus.toml` or constructed by the corpus runner.

## Extended tier

Extended entries (`rustls`, `sqlx`, `bevy`, `cargo`, and `rust-analyzer`) are
evidence-only for 0.2. They remain non-gating until their platform-specific
requirements, runtime, and expected feature surface are pinned. They become
the required tier for the 0.3 milestone.

## Interpretation

This result certifies the pinned required revisions on one platform. It does
not certify every Cargo manifest, every target triple, resolver 3, or the
extended tier. CI must reproduce resolver parity on all supported hosts and
offline builds on the applicable hosts before this evidence can be used as a
0.2 release gate.
