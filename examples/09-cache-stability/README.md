# Cache-stability fixture

This workspace is the checked-in DAG used by `tools/cache-stability.sh`.
It contains independent leaves, a shared dependency, a narrowed build script,
a proc macro, features, a test, a bench, and a target-specific edge.

The harness requires `target/release/tong`, runs correctness assertions on the
exact cached/executed action decisions, and keeps timing secondary. Run it with:

```sh
cargo build --release -p tong
tools/cache-stability.sh
```
