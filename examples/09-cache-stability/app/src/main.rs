fn main() {
    let total = cache_leaf_a::value() + cache_leaf_b::value() + cache_macros::forty_two!();
    println!("{total}");
}
