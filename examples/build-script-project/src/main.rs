include!(concat!(env!("OUT_DIR"), "/generated.rs"));

#[cfg(not(tong_build_script))]
compile_error!("build script cfg was not propagated");

fn main() {
    println!("{}", generated_message());
}

