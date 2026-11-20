use super::{
    ir_ffm_element_bound_site_stays_a_call, ir_ffm_element_call_admits,
    ir_ffm_element_direct_entry, ir_ffm_element_seed_call_site, ir_ffm_element_unreached_site,
};

/// r23 w13 (lane L4, `CRATONVM_JIT_IR_FFM_UNREACHED_SEED_CALL`, opt-in): a
/// site whose trap fired (a speculation at the bci) and that still has no
/// receiver stays a seeded call; with a receiver, without the speculation,
/// or with either switch off, it does not.
/// r23 w11 (lane L4, `CRATONVM_JIT_IR_FFM_UNREACHED_SITE_TRAP`, opt-in): a
/// site is "unreached" only with the switch on, a profile, no receiver
/// observation at its pc, no speculation recorded there, and an element
/// accessor shape.
/// r23 wave 4: a site the compile binds stays a call (the splice planner
/// skips it); unbound (the bind off, no table) it may still splice; `=0`
/// on the keep-call switch restores splicing.
#[test]
fn a_bound_ffm_element_site_is_not_spliced() {
    let table = crate::DirectHelperTable {
        ffm_element_get: 0x1000,
        ffm_element_set: 0x2000,
        static_base_resolver_ctx: 0x3000,
        ..crate::DirectHelperTable::EMPTY
    };
    let seg = "java/lang/foreign/MemorySegment";
    let int_get = "(Ljava/lang/foreign/ValueLayout$OfInt;J)I";
    if crate::ffm_kind_for_descriptor(int_get).is_none() {
        return; // `CRATONVM_JIT_NO_FFM_INTRINSIC=1` in this environment.
    }
    let direct = concat!("CRATON", "VM_JIT_IR_FFM_ELEMENT_DIRECT");
    let keep = concat!("CRATON", "VM_JIT_IR_FFM_ELEMENT_DIRECT_KEEP_CALL");
    let stays = |t: &crate::DirectHelperTable| {
        ir_ffm_element_bound_site_stays_a_call(seg, "getAtIndex", int_get, t)
    };
    cratonvm_types::flags::with_thread_overrides(&[(direct, None), (keep, None)], || {
        assert!(!stays(&table), "retired off");
    });
}

/// r23 wave 3: FFM element direct entry is retired off.
#[test]
fn ffm_element_direct_entry_takes_the_intrinsic_shapes_only() {
    let table = crate::DirectHelperTable {
        ffm_element_get: 0x1000,
        ffm_element_set: 0x2000,
        static_base_resolver_ctx: 0x3000,
        ..crate::DirectHelperTable::EMPTY
    };
    let seg = "java/lang/foreign/MemorySegment";
    let key = concat!("CRATON", "VM_JIT_IR_FFM_ELEMENT_DIRECT");
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        assert_eq!(
            ir_ffm_element_direct_entry(
                seg,
                "getAtIndex",
                "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
                &table
            ),
            None
        );
    });
}

/// The element accessors of `MemorySegment` are retired off.
#[test]
fn ffm_element_sites_are_admitted_as_calls_under_their_switch() {
    let key = concat!("CRATON", "VM_JIT_IR_FFM_ELEMENT_CALL");
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        assert!(!ir_ffm_element_call_admits(
            "java/lang/foreign/MemorySegment",
            "getAtIndex"
        ));
        assert!(!ir_ffm_element_call_admits(
            "java/lang/foreign/MemorySegment",
            "setAtIndex"
        ));
    });
}
