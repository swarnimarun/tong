pub fn value() -> u32 {
    cache_shared::shared() + 1
}
