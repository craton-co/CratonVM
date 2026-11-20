use super::redefined_ancestor_verdict;
use crate::runtime::interpreter::AncestorGuard;

/// A native the door's own immunity keeps is looked up; one kept only for
/// receivers a publishing door cannot tell apart ends the walk (the door
/// caches nothing); anything else is skipped, and the walk goes on to the
/// redefined ancestor's woven bytecode or past it.
#[test]
fn the_verdict_keeps_refuses_or_skips() {
    assert!(matches!(
        redefined_ancestor_verdict(true, false),
        AncestorGuard::Look
    ));
    assert!(matches!(
        redefined_ancestor_verdict(true, true),
        AncestorGuard::Look
    ));
    assert!(matches!(
        redefined_ancestor_verdict(false, true),
        AncestorGuard::EndWalk
    ));
    assert!(matches!(
        redefined_ancestor_verdict(false, false),
        AncestorGuard::SkipNative
    ));
}

/// Every door on the shared walk names itself to `redefined_ancestor_guard`
/// at its call site, so the five cannot grow five guards again
/// (`r15w2-jdkonly-ancestor-walk-redefine-guard-drift-FIXED-20260930.md`). The
/// needle is built at run time, so this module's own text never matches,
/// and the guard's match arms spell the doors `D::…`.
#[test]
fn every_ancestor_walk_door_asks_the_one_redefine_rule() {
    let doors: [(&str, &str, &str); 5] = [
        (
            "dispatch_virtual.rs",
            include_str!("../dispatch_virtual.rs"),
            "Populate",
        ),
        (
            "dispatch_virtual.rs",
            include_str!("../dispatch_virtual.rs"),
            "VtableProbe",
        ),
        ("invoke.rs", include_str!("../invoke.rs"), "Stackless"),
        (
            "vm/vm_exec.rs",
            concat!(include_str!("../../../vm/vm_exec/mod.rs"), "\n", include_str!("../../../vm/vm_exec/method.rs"), "\n", include_str!("../../../vm/vm_exec/report_pad_access.rs"), "\n", include_str!("../../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../../../vm/vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../../../vm/vm_exec/load_class.rs"), "\n", include_str!("../../../vm/vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../../../vm/vm_exec/new_object.rs"), "\n", include_str!("../../../vm/vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../../../vm/vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../../../vm/vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../../../vm/vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_special_shared.rs"), "\n", include_str!("../../../vm/vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../../../vm/vm_exec/jni_table_native_call.rs")),
            "InvokeOrNative",
        ),
        (
            "jit/helpers.rs",
            crate::jit::helpers::HELPERS_SRC,
            "JitSiteCache",
        ),
    ];
    for (file, src, door) in doors {
        let needle = format!("AncestorWalkDoor::{door}");
        let sites = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && t.contains(&needle)
            })
            .count();
        assert_eq!(
            sites, 1,
            "{file}: `{needle}` should name exactly one ancestor-walk call site, found {sites}"
        );
    }
}
