#[test]
fn sums_cross_member() {
    assert_eq!(core::core() + extra::extra(), 10);
}
