use super::{
    mh_constant_bound_kind_admitted as admitted, mh_constant_bound_static_site_matches,
    mh_constant_compile_bound_static_enabled, mh_constant_kind_word,
};


#[test]
fn each_bound_kind_rides_its_own_switch() {
    assert!(admitted(4, true, false));
    assert!(!admitted(4, false, true));
    assert!(admitted(6, false, true));
    assert!(!admitted(6, true, false), "the virtual switch does not admit a static row");
    for kind in [0u8, 2, 3, 5] {
        assert!(!admitted(kind, true, true), "kind {kind} has no bound row");
    }
    assert_eq!(mh_constant_kind_word(6), "bound-static");
}

#[test]
fn the_site_is_the_member_less_its_bound_leading_reference() {
    let m = "(Ljava/lang/Long;I)J";
    assert!(mh_constant_bound_static_site_matches(m, "(I)J"));
    assert!(mh_constant_bound_static_site_matches("(Lp/Box;)V", "()V"));
    // invokeExact at the member's own type, or any other: the door's WMTE.
    assert!(!mh_constant_bound_static_site_matches(m, m));
    assert!(!mh_constant_bound_static_site_matches(m, "(J)J"));
    assert!(!mh_constant_bound_static_site_matches(m, "(I)I"));
    // A primitive or array leading parameter is never a bindTo of a static.
    assert!(!mh_constant_bound_static_site_matches("(JI)J", "(I)J"));
    assert!(!mh_constant_bound_static_site_matches("([II)J", "(I)J"));
}
