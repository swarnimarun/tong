# Hermeticity Model

Tong's core promise: an action observes only its declared inputs. This
chapter defines what "hermetic" means here, what is enforced today on
each platform, and how to turn enforcement on.

## Hermeticity vs reproducibility

Two separate claims (PLAN.md §3.4):

- **Hermetic** — the action *cannot* observe undeclared files,
  environment, or the network.
- **Reproducible** — the action *does* produce equivalent outputs when
  re-run with the same inputs (some hermetic actions are still
  non-reproducible because a compiler embeds timestamps or random
  values).

Tong reports them independently:

```text
Hermeticity: enforced
Reproducibility: verified
Cacheability: enabled
Remote eligibility: enabled
```

A clean environment is the baseline that is always on: actions never
inherit `PATH`, compiler flags, SDK variables, package-manager
configuration, proxy variables, credentials, or user configuration.
Only controlled values (`TMPDIR`, a sandbox `HOME`, locale, the
deterministic working directory) are provided.

## Execution modes

Tong has a general compatibility mode and an opt-in security mode. Cargo
import defaults to `compat`, so ordinary projects can migrate before their
build scripts and proc macros have permission declarations. Compatibility mode
prioritizes Cargo behavior, reports that hermeticity is not enforced, and
keeps unsafe build-script/proc-macro results local-only or uncacheable. It
still uses Tong's content-addressed storage and selected-output materialization.

`hermetic` mode loads reviewed permissions and refuses undeclared access. It is
the mode eligible for hermetic and shared/remote cache claims.

## Enforcement levels

Sandboxing is **opt-in** (`[policy] mode = "hermetic"` plus `sandbox`) until
certified per platform:

| Level | Enforcement | Meaning |
|---|---|---|
| `l1` | Clean environment | Deterministic base env; no extra process isolation. Default |
| `l2` | Clean environment | Currently identical to l1 (no extra enforcement yet) |
| `l3` | Filesystem isolation | Inputs read-only, only output/tmp writable |
| `l4` | Network denial | l3 + network denied |

Only levels 3 and 4 qualify as "hermetic" for general cache
publication. Configure per workspace:

```toml
[policy]
sandbox = "l3"
```

Build scripts and proc macros are treated as untrusted action programs. Use
`tong audit permissions` to trace their attempted file, environment, process,
and network accesses, review the generated candidate, and save approved
capabilities in `Tong.permissions.toml` for Cargo-import workspaces. Normal
builds enforce that file and never prompt in CI; a new access fails with the
exact missing permission. Audit runs are diagnostic only and are not
cache-publishable.

## Platform support

| Platform | l3/l4 mechanism | Status |
|---|---|---|
| Linux | bubblewrap (`bwrap`) | Implemented; requires `bwrap` installed (actionable error otherwise). Read-only binds for the captured toolchain closure, `/usr`, `/lib`, `/etc`; writable only out/tmp |
| macOS | Seatbelt (`sandbox-exec`) | Implemented, but `sandbox-exec` is deprecated and its deny-default profiles abort at exec time on Sequoia (macOS 15)+; on those releases l3/l4 builds fail with an actionable error and the platform is documented as **l1/l2-only** |
| Windows | — | Documented no-op beyond the clean environment (l1/l2 only) |

The captured toolchain's closure (registered bundle roots and system
tool parents) is bound read-only so sandboxed actions can still run
rustc; the network policy of the action is threaded into the sandbox
spec.

Levels are a release gate, not a marketing claim: a platform is
advertised as hermetic only after its enforcement is certified, and the
[capability matrix](roadmap.html) tracks where each level stands.

## What stays hermetic regardless of sandbox level

Even at l1, the pipeline itself is pure and deterministic:

- Analysis never reads undeclared files, the network, the host
  environment, or the clock — the action graph is a pure function of
  the workspace inputs.
- Action environments are built from scratch, never inherited.
- Toolchain identity is a content fingerprint, not a version string:
  the sysroot is hashed (then snapshot-cached per machine) and a
  changed byte invalidates every action that used it.
- Action digests cover the semantic execution fields, the input tree
  digest, and the environment bundle digest — no absolute host paths,
  no logical ids, no target names.

The [adversarial test suite](roadmap.html) (attempted home-directory
reads, undeclared writes, network access, time dependence, leftover
child processes) is the enforcement of last resort: each level's claims
are tested, not assumed.
