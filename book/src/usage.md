# Usage Guide

Tong discovers either `Tong.toml` or `Cargo.toml` from the path you pass to a
command. If the path is omitted, discovery starts in the current directory.

## Commands

```sh
tong build [OPTIONS] [PATH]
tong fetch [OPTIONS] [PATH]
tong plan [OPTIONS] [PATH]
tong add NAME SOURCE [OPTIONS]
tong clean [PATH]
```

`tong build` loads the manifest graph, lowers Rust targets into explicit
actions, and writes outputs under `target/tong`.

`tong fetch` resolves and materializes dependency sources without compiling.

`tong plan` prints the packages, targets, build scripts, and dependencies Tong
discovered.

`tong add` inserts a git, tar, or zip dependency entry into the selected
manifest.

`tong clean` removes the selected package's `target/tong` directory.

## Common Options

```sh
--manifest-path PATH
--release
--debug
-v, --verbose
```

## Adding Source Dependencies

```sh
cargo run -p tong -- add clap \
  --tar https://static.crates.io/crates/clap/clap-4.5.54.crate \
  --sha256 <sha256> \
  --features derive,std \
  --no-default-features \
  --manifest-path examples/cli-mini/Tong.toml
```
