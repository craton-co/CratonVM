use super::*;

#[inline(never)]
fn jdk_native(x: usize) -> usize {
    x.wrapping_mul(31).wrapping_add(7)
}
#[inline(never)]
fn app_native(x: usize) -> usize {
    x.wrapping_mul(17).wrapping_add(3).rotate_left(5)
}

#[test]
fn suppress_nests_and_restores() {
    assert!(!suppressed());
    suppress(|| {
        assert!(suppressed());
        suppress(|| assert!(suppressed()));
        assert!(suppressed());
    });
    assert!(!suppressed());
}

#[test]
fn only_an_application_only_callback_is_an_application_native() {
    let app = app_native as usize;
    let jdk = jdk_native as usize;
    note_registration("org/h2/value/ValueBigint", app);
    note_registration("java/lang/Class", jdk);
    {
        let _f = enter_native(app);
        assert!(current_native_is_application());
    }
    {
        let _f = enter_native(jdk);
        assert!(!current_native_is_application());
    }
    // Registered for both: conservatively a JDK native.
    note_registration("org/h2/value/ValueBigint", jdk);
    {
        let _f = enter_native(jdk);
        assert!(!current_native_is_application());
    }
    assert!(!current_native_is_application());
}
