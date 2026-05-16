# Quick Start

Build Tong:

```sh
cargo build -p tong
```

Build the sample project:

```sh
cargo run -p tong -- build examples/simple-rust-project
```

Run the produced binary:

```sh
./examples/simple-rust-project/target/tong/debug/bin/hello-tong
```

Build and run the default binary in one step:

```sh
cargo run -p tong -- run examples/simple-rust-project
```

Inspect the package graph:

```sh
cargo run -p tong -- plan examples/simple-rust-project
```

Build with release flags:

```sh
cargo run -p tong -- build --release examples/simple-rust-project
```
