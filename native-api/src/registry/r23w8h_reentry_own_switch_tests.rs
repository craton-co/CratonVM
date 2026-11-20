use super::NativeMethodRegistry;

/// A row marked on its own switch is in both lists, once.
#[test]
fn an_own_switch_row_is_also_an_ordinary_reentry_row_once() {
    let mut r = NativeMethodRegistry::new();
    let (c, m, d) = ("p/L", "loadClassOrNull", "(Ljava/lang/String;Z)Ljava/lang/Class;");
    r.mark_reentry_only_body_on_its_own_switch(c, m, d);
    r.mark_reentry_only_body_on_its_own_switch(c, m, d);
    r.mark_reentry_only_body("p/H", "asType", "()V");
    assert_eq!(r.reentry_only_bodies_own_switch(), &[(c, m, d)]);
    assert_eq!(r.reentry_only_bodies(), &[(c, m, d), ("p/H", "asType", "()V")]);
}
