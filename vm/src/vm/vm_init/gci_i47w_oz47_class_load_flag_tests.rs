//! gci i47w lane oz47: the per-name loading flag of
//! `SharedVm::load_class_concurrent_for` is cleared, and its waiters woken,
//! BEFORE the class-manager write guard's drop drains the JVMTI
//! `ClassLoad` hooks (a C callback may wait GC-safe and then wait out a
//! pause; a waiter on the flag is not GC-safe).

/// Source pin: the flag guard is taken out and dropped before `cm_guard`.
#[test]
fn gci_i47w_oz47_loading_flag_cleared_before_the_class_load_drain() {
    let src = concat!(include_str!("../vm_init/mod.rs"), "\n", include_str!("../vm_init/missing_native_entry.rs"), "\n", include_str!("../vm_init/real_jdk_to_array_typed.rs"), "\n", include_str!("../vm_init/zgc_relocation_permitted.rs"), "\n", include_str!("../vm_init/wait_site.rs"), "\n", include_str!("../vm_init/class_store_cut_with.rs")).replace("\r\n", "\n");
    let start = src
        .find("pub fn load_class_concurrent_for(")
        .expect("load_class_concurrent_for");
    let rest = &src[start..];
    let body = &rest[..rest.find("\n    \u{7d}\n").expect("its end")];
    let flag = body
        .find("cm_guard.run_before_hooks(move || drop(flag));")
        .expect("the flag released between the lock release and the drain");
    let guard = body.find("drop(cm_guard);").expect("the guard drop");
    let load = body.find("cm_guard.load_class(name)").expect("the load");
    assert!(load < flag && flag < guard, "load, then flag hand-off, then release");
    // Never under the write lock (the i48h lock-order deadlock).
    assert!(!body.contains(concat!("drop(loading_guard", ".take());")));
    assert!(body.contains("let mut loading_guard = Some(LoadingFlagGuard {"));
}
