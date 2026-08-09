# AGENTS.md

Instructions for AI coding agents working in this repository. Read this file before making changes.

## Scope

Applies to the whole repository. More specific rules live in nested `AGENTS.md` files (see [Nested instructions](#nested-instructions)); when instructions conflict, the most specific file wins.

## Critical constraints

- MUST NOT commit secrets, credentials, tokens, or local environment files (`.env*` is gitignored).
- MUST NOT edit generated files, lockfiles (`Cargo.lock`), or vendored code by hand — always regenerate through the tool that owns them.
- MUST run the validation commands in [Commands](#commands) (`just check`, `just lint`, `just test`, `just fmt --check`) before requesting review.
- MUST NOT add dependencies without explaining why in the commit message.
- MUST NOT weaken or delete tests to make them pass.
- ALWAYS keep diffs focused on the task at hand.
- ALWAYS treat `PLAN.md` as the governing design document; design deviations must be discussed with the user before implementation.

## Project overview

Tong is a declarative, hermetic, multi-language build system for monorepos that preserves familiar language workflows while lowering all build work into explicit, cacheable actions. Implemented in Rust as a Cargo workspace; `cargo` is the package manager, `just` wraps the common commands. `PLAN.md` holds the full reimplementation and delivery plan (design principles, action model, phases).

## Repository structure

- `PLAN.md` — governing design and delivery plan; read the relevant section before non-trivial work
- `tong/` — CLI binary crate (build/run/clean driver: manifests → toolchain → plan → schedule → assemble)
- `tong-core/` — action schema, artifacts, canonical encoding/decoding, digests, platforms, providers
- `tong-graph/` — `Tong.toml` manifest model, labels, planned actions, topological scheduling
- `tong-rust/` — Rust backend: target model, system toolchain capture, `Cargo.toml` import, action planning, build scripts, proc macros, `cc_import`
- `tong-exec/` — local process executor: deterministic exec roots, clean env, output validation
- `tong-store/` — content-addressed store, tree capture/materialize, bundle storage, action cache
- `examples/` — runnable workspaces (`01-hello`, `01-calc`, `02-advanced`, `03-sdl3`, `04-voxel-city`, `05-voxel-city-cargo`, `06-sdl3-cargo`)

## Setup and prerequisites

- Rust toolchain: pinned in `rust-toolchain.toml` (stable, with `clippy` + `rustfmt`); rustup auto-selects it
- `just` command runner required for the recipes below
- Bootstrap: `just setup`
- `examples/03-sdl3` additionally needs SDL3 from Homebrew (`brew install sdl3`); see its README

## Commands

All run from the repository root.

```sh
just setup      # install pinned toolchain components (idempotent)
just build      # cargo build --workspace
just check      # cargo check --workspace --all-targets (fast type-check)
just test       # cargo test --workspace
just lint       # cargo clippy --workspace --all-targets -- -D warnings
just fmt        # cargo fmt --all (check-only: just fmt --check)
just run -- ... # run the tong CLI with args
```

One test: `cargo test -p <crate> <test-name>`. None of the commands need the network after `just setup`.

## Architecture and boundaries

The pipeline is: manifests → resolution → configured target graph → backend analysis → immutable action graph → execution → content-addressed outputs (`PLAN.md` §1). Enforce these invariants in code:

- Actions are the only execution unit; backends never invoke compilers or package managers as ambient subprocesses during analysis (`PLAN.md` §3.1).
- Analysis is pure: no undeclared file reads, no network, no host environment inspection, no time/RNG dependence (`PLAN.md` §3.2). Analysis outputs planned actions whose specs are concretized at schedule time — never cache under a key computed before the complete input set is known (§8.3).
- Action digests cover semantic execution fields only — never `logical_id`, target names, or absolute host paths (`PLAN.md` §4). Exec args use `{exec_root}`/`{bundle_root}` placeholders substituted by the executor.
- Dependency direction: `tong-core` ← `tong-graph`/`tong-rust` ← `tong-exec`; `tong-store` is standalone; `tong` (CLI) sits on top. Do not add reverse or circular crate dependencies.
- System-captured toolchains (rustc) and `cc_import` libraries are non-portable (PLAN.md §5): fingerprinted into the action digest via properties (`tong.rust.rustc_verbose_version`, `tong.rust.sysroot_tree`, `tong.execution.portable=false`), used in place, never published to shared caches.
- Cargo manifests are imported (`tong-rust::cargo_import`), never invoked: no ambient cargo; registry/git deps fail with a targeted diagnostic until Phase 3 locking exists.

## Code style and conventions

- Edition 2024; formatting and lint are enforced by `just fmt --check` and `just lint` — run them instead of hand-formatting.
- No `unsafe` without a comment justifying it; prefer fallible APIs (`Result`) over `unwrap`/`expect` outside tests.
- Crate metadata (version, edition, license) is inherited from the workspace root — do not repeat it in member manifests.
- Schemas that affect digests or caches are versioned; changing one is a breaking change and needs an explicit schema-version bump (`PLAN.md` §4.5).

## Testing

- Unit tests live next to the code (`#[cfg(test)]` modules); integration tests live in each crate's `tests/` directory.
- Run the targeted test first (`cargo test -p <crate> <name>`), then `just test` before finishing.
- Behavior changes require test updates in the same change; never weaken or delete a failing test — investigate or ask.
- Digest/hashing code needs golden cross-platform test vectors (`PLAN.md` §16); sandbox and cache code needs the adversarial and cache-correctness tests described there.
- The example workspaces in `examples/` are the end-to-end acceptance suite: after changing the backend or driver, rebuild each of them (`tong build` from the example dir) and check they still run.
- `examples/03-sdl3` needs SDL3 from Homebrew; use `SDL_VIDEODRIVER=dummy` for headless runs.

## Do / Don't

- Do add or update tests for behavior changes.
- Do run the targeted test before the full suite.
- Do consult `PLAN.md` before introducing new action fields, cache keys, or execution behavior.
- Don't edit generated output or `Cargo.lock` directly — regenerate it.
- Don't commit secrets or local environment files.
- Don't add dependencies without justification in the commit message.
- Don't make unrelated refactors in the same change.
- Don't push to remotes or run destructive VCS operations without asking — branch creation and non-destructive ops are fine.

## Git and pull requests

VCS is **jj (Jujutsu)** colocated with git (both `.jj/` and `.git/` exist; use jj commands).

- The working copy is a commit that auto-snapshots — there is no staging area; `.gitignore` controls what jj snapshots.
- Commit after each completed change, before starting the next one. Prefer jj (`jj describe -m` — the working copy is already a commit; `jj commit` when a separate commit is needed); fall back to git (`git add` + `git commit`) only when jj is unavailable; if neither works, ask the user for help.
- Agents MAY create branches and run non-destructive operations (`jj new`, `jj describe`, `jj commit`, `jj branch create`, `jj split`, `jj squash`).
- Agents MUST NOT push (`jj git push`) — pushing is human-only.
- Agents MUST ask before destructive operations (`jj abandon`, `jj op restore`, `jj gc`, `git reset --hard`, `git clean -f`, deleting branches).
- Commit messages use Conventional Commits titles: `<type>: <description>` (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`, `build:`, `ci:`), full summary ≤50 chars including the type, imperative mood, lowercase description, no trailing period, one logical change; blank line, then a body wrapped at ≤72 chars explaining what and why, not how. Set with `jj describe -m`.
- Required checks before a change is done: `just check`, `just lint`, `just test`, `just fmt --check`.

## Subagent conventions

- Web research / anything requiring web search MUST use the `web-researcher` agent (`openai-codex/gpt-5.6-luna`, high thinking) — see `~/.pi/agent/AGENTS.md`.
- Fallback ONLY when `web-researcher` is genuinely unavailable (attempt it first): read-only local-tool research (`curl`, `gh`, raw.githubusercontent.com, DuckDuckGo HTML) — see `~/.pi/agent/AGENTS.md`.
- Research and review subagents are read-only: they search and report, they do not modify the repository.
- Keep one writer at a time for implementation work; use reviewers/validators as independent read-only passes.
- Subagent run artifacts live under `.pi-subagents/`, which is gitignored — do not commit them.

## References

- `PLAN.md` — governing design document: principles (§3), action model (§4), phases (§15), testing strategy (§16)

## Nested instructions

No nested `AGENTS.md` files yet. Add one per crate (`tong-*/AGENTS.md`) only when a subtree needs its own rules; nested files must only add or refine rules for their subtree.
