# Introduction

Tong is an experimental Rust-first hermetic build system. The project explores
what a direct, explicit build graph for Rust can look like when compile actions
are described by their inputs, outputs, environment, toolchain, and command
line.

The 0.1.0 release is an MVP. It can build simple Rust libraries and binaries
from `Cargo.toml` or `Tong.toml`, materialize fixed source dependencies, run
basic build scripts, and cache actions under `target/tong`.

Tong is not a complete security sandbox yet. Its current hermeticity boundary is
the action model: clean environments, declared inputs and outputs, and cache
keys that include relevant command and platform material.
