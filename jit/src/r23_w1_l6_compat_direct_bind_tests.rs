use super::{direct_native_helper, direct_native_helper_for_impl};

const ENTRY: usize = 0x1234;

#[test]
fn compat_binds_only_what_the_vm_admits() {
    let refuse_all = |_: &str, _: &str, _: &str| false;
    let admit_all = |_: &str, _: &str, _: &str| true;
    assert_eq!(direct_native_helper(ENTRY, false, Some(&refuse_all), "a/B", "m", "()V"), 0);
    assert_eq!(
        direct_native_helper(ENTRY, false, Some(&admit_all), "a/B", "m", "()V"),
        ENTRY
    );
    // No admission closure (hand-built request): the old unconditional bind.
    assert_eq!(direct_native_helper(ENTRY, false, None, "a/B", "m", "()V"), ENTRY);
    // An unwired cell stays unwired without asking.
    let panics = |_: &str, _: &str, _: &str| -> bool { panic!("not asked for a zero cell") };
    assert_eq!(direct_native_helper(0, false, Some(&panics), "a/B", "m", "()V"), 0);
}

#[test]
fn compat_for_impl_asks_the_implementing_row() {
    let asked = std::cell::RefCell::new(Vec::<String>::new());
    let only_impl = |c: &str, _: &str, _: &str| {
        asked.borrow_mut().push(c.to_string());
        c == "java/util/concurrent/ConcurrentHashMap"
    };
    let entry = direct_native_helper_for_impl(
        ENTRY,
        false,
        Some(&only_impl),
        "java/util/concurrent/ConcurrentMap",
        "java/util/concurrent/ConcurrentHashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    );
    assert_eq!(entry, ENTRY);
    assert_eq!(*asked.borrow(), vec!["java/util/concurrent/ConcurrentHashMap".to_string()]);
}
