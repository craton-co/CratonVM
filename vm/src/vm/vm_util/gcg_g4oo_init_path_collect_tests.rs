use super::{init_path_collect_safe, init_path_may_collect, ClassId, SharedVm};

/// gcg g4/oo: outside a native callback the init path may collect without
/// asking about `<clinit>`; inside one only when Java runs anyway.
#[test]
fn gcg_g4oo_init_path_collects_outside_a_native_or_when_java_runs() {
    let asked = std::cell::Cell::new(false);
    assert!(init_path_collect_safe(false, || {
        asked.set(true);
        false
    }));
    assert!(!asked.get(), "not asked outside a native");
    assert!(init_path_collect_safe(true, || true));
    assert!(!init_path_collect_safe(true, || false));
}

/// The native scope is the one `native_oom` grants: a class with no
/// `<clinit>` (here: not loaded at all) is refused inside it only.
#[test]
fn gcg_g4oo_a_native_scope_without_a_clinit_does_not_collect() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let absent = ClassId::new(0x7fff_fff0);
    assert!(init_path_may_collect(&shared, absent));
    let inside =
        crate::runtime::native_oom::catch_alloc_oom(|| init_path_may_collect(&shared, absent));
    assert!(matches!(inside, Ok(false)));
}

/// Both init-path collections ask the guard before they collect.
#[test]
fn gcg_g4oo_both_init_path_reclaims_are_guarded() {
    let src = include_str!("../vm_util.rs");
    for head in ["fn link_claimed_class(", "fn create_mirror_on_init_path("] {
        let body = src
            .split(head)
            .nth(1)
            .and_then(|rest| rest.split("\nfn ").next())
            .expect("function body");
        let guard = body
            .find("init_path_may_collect(shared, class_id)")
            .expect("the guard");
        let reclaim = body
            .find("native_reclaim_before_alloc_retry_graced(")
            .expect("the reclaim");
        assert!(guard < reclaim, "{head}: the guard precedes the reclaim");
    }
}
