//! Locates the Homebrew SDL3 and emits the native-link directives.
//!
//! Only used when this crate is built as a Cargo package (e.g. by
//! `examples/06-sdl3-cargo`); the Tong.toml targets in 03-sdl3 and
//! 04-voxel-city link SDL3 through their `cc_import` targets instead and
//! never run this script. The search path is a host path — non-portable
//! by design, like tong's system toolchain capture; the runtime library
//! is loaded through its absolute install name.

use std::path::Path;

fn main() {
    let prefix = std::env::var("SDL3_DIR")
        .unwrap_or_else(|_| "/opt/homebrew/opt/sdl3".to_owned());
    let lib = Path::new(&prefix).join("lib");
    if !lib.join("libSDL3.0.dylib").exists() {
        panic!(
            "SDL3 not found in {}: install it with `brew install sdl3` \
             or point SDL3_DIR at the prefix",
            lib.display()
        );
    }
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=SDL3.0");
    println!("cargo:rerun-if-env-changed=SDL3_DIR");
    println!("cargo:rerun-if-changed=build.rs");
}
