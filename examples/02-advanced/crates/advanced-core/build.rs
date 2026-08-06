//! Build script: generates deterministic constants from a data file.
//!
//! Everything here is content-addressed: the input is `data/message.txt`,
//! the outputs are `OUT_DIR/message.rs` and the `ADVANCED_MESSAGE` env var.
//! No timestamps or random values — hermetic builds only.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let message = fs::read_to_string("data/message.txt").expect("data/message.txt");

    // Emit a generated source file the crate will include!().
    fs::write(
        out_dir.join("message.rs"),
        format!("pub const MESSAGE: &str = {message:?};\n"),
    )
    .expect("write OUT_DIR/message.rs");

    // Emit a compile-time environment variable.
    println!("cargo:rustc-env=ADVANCED_MESSAGE={message}");

    // Deterministic native-like flags for demonstration.
    println!("cargo:rustc-cfg=has_build_script");
    println!("cargo:warning=build script generated constants");
}
