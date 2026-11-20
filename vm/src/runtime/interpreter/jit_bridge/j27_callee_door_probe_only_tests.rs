use super::CalleeDoorMiss;

/// j27 wave 3 (lane DISPATCH2): what each door mode may do on a miss.
#[test]
fn j27_callee_door_miss_modes() {
    assert!(CalleeDoorMiss::MayDefer.may_defer() && CalleeDoorMiss::MayDefer.may_compile());
    assert!(
        !CalleeDoorMiss::CompileInPlace.may_defer(),
        "the OSR door's binds"
    );
    assert!(CalleeDoorMiss::CompileInPlace.may_compile());
    assert!(!CalleeDoorMiss::ProbeOnly.may_defer());
    assert!(!CalleeDoorMiss::ProbeOnly.may_compile());
}

/// The probe-only stop sits after every probe and filter and before the
/// deferral and the slow compile, and records no negative verdict (source
/// check of `try_jit_compile_callee_uncontained`).
#[test]
fn j27_probe_only_stops_before_defer_and_compile_without_a_verdict() {
    let whole = concat!(
        include_str!("../jit_bridge/mod.rs"),
        "\n",
        include_str!("../jit_bridge/jit_verdicts.rs"),
        "\n",
        include_str!("../jit_bridge/osr_continuation.rs"),
        "\n",
        include_str!("../jit_bridge/compile_osr_body.rs"),
        "\n",
        include_str!("../jit_bridge/osr_exception_exit.rs"),
        "\n",
        include_str!("../jit_bridge/try_osr.rs"),
        "\n",
        include_str!("../jit_bridge/native_shadow_arms.rs"),
        "\n",
        include_str!("../jit_bridge/compile_optimizing_artifact.rs"),
        "\n",
        include_str!("../jit_bridge/never_run_static_exit_pcs.rs"),
        "\n",
        include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"),
        "\n",
        include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"),
        "\n",
        include_str!("../jit_bridge/folded_handle_member_row.rs"),
        "\n",
        include_str!("../jit_bridge/callee_never_taken_rows.rs"),
        "\n",
        include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")
    )
    .replace("\r\n", "\n");
    let src = &whole;
    let start = src
        .find("fn try_jit_compile_callee_uncontained(")
        .expect("the door");
    let body = &src[start..];
    let body = &body[..body.find("\n\u{7d}\n").expect("fn end")];
    let stop = body.find("if !on_miss.may_compile() {").expect("the stop");
    let probe = body
        .find("jit_cache.get(class_name")
        .expect("the cache probe");
    let sync = body
        .find("named_method_is_synchronized(")
        .expect("the sync filter");
    let defer = body
        .find("defer_callee_compile_to_worker(")
        .expect("the deferral");
    let slow = body
        .find("try_jit_compile_callee_slow(")
        .expect("the slow path");
    assert!(probe < stop && sync < stop, "filters and probe come first");
    assert!(
        stop < defer && stop < slow,
        "nothing is deferred or compiled"
    );
    let arm = &body[stop..];
    let arm = &arm[..arm.find("return None;").expect("the stop answers None")];
    assert!(!arm.contains("slot.store"), "no negative verdict");
}
