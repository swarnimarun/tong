fn main() {
    println!("cargo:rerun-if-changed=src/main.rs");
    println!("cargo::rustc-link-search=native=mylib");
    println!("cargo::rustc-link-lib=static:mylib");
    println!("cargo::rustc-env=GENERATED_VERSION=compat");
    println!("cargo::rustc-link-arg=-Wl,-dead_strip");
}
