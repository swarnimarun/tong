fn main() {
    println!("cargo::rustc-env=BUILD_HELPER_MESSAGE={}", build_helper::message());
}
