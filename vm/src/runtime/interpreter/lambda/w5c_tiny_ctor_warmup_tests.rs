use super::tiny_ctor_tier_up_threshold as t;

#[test]
fn only_a_tiny_init_without_an_explicit_threshold_warms_early() {
    assert_eq!(t(500, "<init>", 20, false), 200);
    assert_eq!(t(2_000, "<init>", 48, false), 200);
    assert_eq!(t(500, "<init>", 49, false), 500);
    assert_eq!(t(500, "get", 5, false), 500);
    assert_eq!(t(500, "<init>", 20, true), 500);
    assert_eq!(t(100, "<init>", 20, false), 100);
}

