/// The arm asks the memo before `JitCache::get`, parks the body after the
/// call, and the dropped-VM hook empties it. A memo that is read but never
/// parked (or parked but never released) is a silent regression or leak.
#[test]
fn the_arm_takes_parks_and_releases_the_body() {
    let src = include_str!("../dispatch_virtual.rs");
    let production = src
        .split("mod w3b_virtual_body_memo_tests")
        .next()
        .unwrap_or(src);
    let take = production.find("take_virtual_body_memo(&cached, jit_generation)");
    let probe = production.find("found.map(cratonvm_jit::RetainedCode::new)");
    assert!(
        take.is_some() && probe.is_some() && take < probe,
        "the memo is consulted before the string-keyed probe"
    );
    assert!(production.contains("park_virtual_body_memo(VirtualBodyMemo {"));
    let hook = concat!(include_str!("../../../jit/helpers/thread/mod.rs"), "\n", include_str!("../../../jit/helpers/thread/direct_static_compiled_callee_entry.rs"), "\n", include_str!("../../../jit/helpers/thread/class_was_redefined.rs"), "\n", include_str!("../../../jit/helpers/thread/jit_service_callee_deopt_body.rs"), "\n", include_str!("../../../jit/helpers/thread/decode_dispatch_values_into_shaped.rs"));
    assert!(hook.contains("release_virtual_body_memo()"));
}

/// The ways are a power of two, so the mask in `virtual_body_memo_way` is
/// a modulo.
#[test]
fn the_way_count_is_a_power_of_two() {
    assert!(super::VIRTUAL_BODY_MEMO_WAYS.is_power_of_two());
}
