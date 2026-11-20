use super::dispatch_counter_compile_is_inline;

#[test]
fn j27_dispatch_counter_leaves_the_compile_to_the_manager_under_bg() {
    assert!(!dispatch_counter_compile_is_inline(true, true), "default: manager");
    assert!(dispatch_counter_compile_is_inline(true, false), "=0 kill: inline");
    assert!(dispatch_counter_compile_is_inline(false, true), "BG_COMPILE=0: inline");
    assert!(dispatch_counter_compile_is_inline(false, false));
}


/// The static tail's inline compile is behind the decision (source
/// check: the crossing is the only place it is asked).
#[test]
fn j27_static_dispatch_counter_crossing_asks_the_policy() {
    let src = concat!(include_str!("../dispatch/mod.rs"), "\n", include_str!("../dispatch/mh_constant_bind_enabled.rs"), "\n", include_str!("../dispatch/mh_constant_bind_fill.rs"), "\n", include_str!("../dispatch/mh_constant_filter_compile_fill.rs"), "\n", include_str!("../dispatch/jit_invoke_dispatch_body.rs"), "\n", include_str!("../dispatch/try_jit_site_cached_native_dispatch.rs"), "\n", include_str!("../dispatch/static_dispatch_counter_compiles.rs")).replace("\r\n", "\n");
    let at = src
        .find("*count == crate::runtime::env_cache::jit_invocation_threshold()\n        })")
        .expect("the DISPATCH_COUNTER crossing");
    let tail = src.get(at..at + 400).expect("ASCII window after the crossing");
    assert!(tail.contains("&& static_dispatch_counter_compiles_inline();"));
}
