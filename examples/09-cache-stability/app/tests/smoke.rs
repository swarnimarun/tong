#[test]
fn shared_branches_link() {
    assert_eq!(cache_leaf_a::value() + cache_leaf_b::value(), 11);
}
