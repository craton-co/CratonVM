use super::mh_bound_static_compile_row_wanted as wanted;

#[test]
fn only_an_unconverted_bound_static_takes_the_row() {
    assert!(wanted(true, true, true, 3));
    assert!(!wanted(true, true, true, 0), "a bound virtual is the kind 4 row");
    assert!(!wanted(true, true, false, 3), "an unbound static is the plain kind 3 row");
    assert!(!wanted(true, false, true, 3), "a converting handle calls no member as it is");
    assert!(!wanted(false, true, true, 3), "no leaf, no memo or a carrier");
}

/// The fill asks the switch and records through the kind 6 writer, after
/// the virtual bound block (source check).
#[test]
fn the_fill_records_the_bound_static_row() {
    let src = concat!(include_str!("../dispatch/mod.rs"), "\n", include_str!("../dispatch/mh_constant_bind_enabled.rs"), "\n", include_str!("../dispatch/mh_constant_bind_fill.rs"), "\n", include_str!("../dispatch/mh_constant_filter_compile_fill.rs"), "\n", include_str!("../dispatch/jit_invoke_dispatch_body.rs"), "\n", include_str!("../dispatch/try_jit_site_cached_native_dispatch.rs"), "\n", include_str!("../dispatch/static_dispatch_counter_compiles.rs")).replace("\r\n", "\n");
    let fill = src.find("unsafe fn mh_constant_bind_fill(").expect("the fill");
    let body = &src[fill..];
    let virt = body
        .find("note_mh_constant_target_bound(")
        .expect("the kind 4 row");
    let stat = body
        .find("note_mh_constant_target_bound_static(")
        .expect("the kind 6 row");
    let gate = body
        .find("mh_constant_compile_bound_static_enabled()")
        .expect("the switch");
    assert!(virt < gate && gate < stat);
}
