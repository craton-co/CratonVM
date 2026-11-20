use super::*;

#[test]
fn the_table_has_no_duplicates_and_answers_its_own_names() {
    for (i, a) in SYNTHETIC_IMPLEMENTS_TARGETS.iter().enumerate() {
        assert!(synthetic_implements_may_admit(a), "{a}");
        for b in &SYNTHETIC_IMPLEMENTS_TARGETS[i + 1..] {
            assert_ne!(a, b);
        }
    }
    for other in [
        "",
        "java/util/HashMap",
        "java/lang/Runnable",
        "[I",
        "Foo$Bar",
    ] {
        assert!(!synthetic_implements_may_admit(other), "{other}");
    }
}
