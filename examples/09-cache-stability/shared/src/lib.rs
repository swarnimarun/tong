pub fn shared() -> u32 {
    if cfg!(feature = "loud") { 40 } else { 4 }
}
