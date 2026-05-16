# Examples

Build a project with a `Tong.toml` manifest:

```sh
cargo run -p tong -- build examples/tong-manifest-project
cargo run -p tong -- run examples/tong-manifest-project
```

Build a project with a local path dependency:

```sh
cargo run -p tong -- build examples/path-dep-project
```

Build a project with a hermetic `build.rs` action:

```sh
cargo run -p tong -- build examples/build-script-project
```

Build a small CLI with fixed source dependencies:

```sh
cargo run -p tong -- build examples/cli-mini
./examples/cli-mini/target/tong/debug/bin/cli-mini echo hello
./examples/cli-mini/target/tong/debug/bin/cli-mini cat README.md
```

Prefetch the same dependency sources without compiling:

```sh
cargo run -p tong -- fetch examples/cli-mini
```
