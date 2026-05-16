# Manifest Reference

Tong reads `Tong.toml` and a small compatible subset of `Cargo.toml`.

## Package

```toml
[package]
name = "hello-tong"
version = "0.1.0"
edition = "2024"
```

## Targets

```toml
[lib]
name = "hello_tong"
path = "src/lib.rs"

[[bin]]
name = "hello-tong"
path = "src/main.rs"
```

Proc macro libraries are supported with:

```toml
[lib]
proc-macro = true
```

## Dependencies

Path dependencies:

```toml
[dependencies]
greet = { path = "greet" }
```

Fixed source dependencies:

```toml
[dependencies]
from_git = { git = "https://example.com/repo.git", rev = "abc123" }
from_tar = { tar = "https://example.com/pkg.tar.gz", sha256 = "..." }
from_zip = { zip = "https://example.com/pkg.zip", sha256 = "..." }
```

Registry-style transitive dependencies need explicit source overrides:

```toml
[tong.sources.clap]
tar = "https://static.crates.io/crates/clap/clap-4.5.54.crate"
sha256 = "..."
```

## Tong Extension

A `Tong.toml` file can extend another manifest:

```toml
[tong]
extends = "Cargo.toml"
```
