use super::virtual_dispatch_counter_door_miss_for;
use crate::runtime::interpreter::jit_bridge::CalleeDoorMiss;

#[test]
fn j27_virtual_dispatch_counter_only_probes_under_bg() {
    assert_eq!(
        virtual_dispatch_counter_door_miss_for(true, true),
        CalleeDoorMiss::ProbeOnly,
        "default: the manager compiles"
    );
    assert_eq!(
        virtual_dispatch_counter_door_miss_for(true, false),
        CalleeDoorMiss::MayDefer,
        "=0 kill: the door defers or compiles as before"
    );
    assert_eq!(
        virtual_dispatch_counter_door_miss_for(false, true),
        CalleeDoorMiss::MayDefer,
        "BG_COMPILE=0: the mutator is the compiler"
    );
}


/// The crossing asks the door through the policy (source check).
#[test]
fn j27_virtual_dispatch_counter_crossing_asks_the_policy() {
    let src = concat!(include_str!("../dispatch/mod.rs"), "\n", include_str!("../dispatch/mh_constant_bind_enabled.rs"), "\n", include_str!("../dispatch/mh_constant_bind_fill.rs"), "\n", include_str!("../dispatch/mh_constant_filter_compile_fill.rs"), "\n", include_str!("../dispatch/jit_invoke_dispatch_body.rs"), "\n", include_str!("../dispatch/try_jit_site_cached_native_dispatch.rs"), "\n", include_str!("../dispatch/static_dispatch_counter_compiles.rs")).replace("\r\n", "\n");
    let at = src
        .find("let count = VIRTUAL_DISPATCH_COUNTER.with(|dc| {")
        .expect("the VIRTUAL_DISPATCH_COUNTER crossing");
    let door = at + src[at..]
        .find("try_jit_compile_callee_for_class_on_miss(")
        .expect("the crossing's door");
    let mode = door + src[door..]
        .find("virtual_dispatch_counter_door_miss(),")
        .expect("the door's mode");
    let close = door
        + src[door..]
            .find(".or_else(|| probe_selected_declaring_body(")
            .expect("the door's call ends before the declaring-body probe");
    assert!(mode < close, "the mode is the door's own argument");
}
