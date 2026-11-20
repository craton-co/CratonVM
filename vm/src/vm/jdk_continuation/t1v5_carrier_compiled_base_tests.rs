use super::*;

#[test]
fn t1v5_a_dropped_set_aside_restores_the_carrier_records() {
    let outer_depth = crate::jit::helpers::replace_jit_dispatch_depth(7);
    let outer_mark =
        crate::runtime::exceptions::replace_resumed_compiled_frame_depth((5, 9, true));
    let base_sp = (crate::jit::conservative_roots::current_stack_pointer() + 2048) & !7;
    let unwound = std::panic::catch_unwind(|| {
        let aside = CarrierCompiledBase::enter(base_sp, None);
        assert_eq!(crate::jit::helpers::replace_jit_dispatch_depth(0), 0, "the slice's");
        assert_eq!(aside.stash_frames(), 0);
        panic!("unwind past the set-aside");
    });
    assert!(unwound.is_err());
    assert_eq!(cratonvm_gc::shadow_stack::current_segment_top(), 0, "segment left");
    assert_eq!(
        crate::jit::helpers::replace_jit_dispatch_depth(outer_depth),
        7,
        "the carrier's dispatch depth is back"
    );
    assert_eq!(
        crate::runtime::exceptions::replace_resumed_compiled_frame_depth(outer_mark),
        (5, 9, true),
        "the carrier's resumed mark is back"
    );
}
