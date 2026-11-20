use super::{
    native_mirror_reclaim_enabled, native_try_mirror_answer, NativeTryMirror, ObjectRef,
};

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: non-null, 8-byte aligned, never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// A mirror passes through and the switch is not read on that path.
#[test]
fn gci_i22oo_a_mirror_passes_through_without_reading_the_switch() {
    let m = fake(0x2000);
    assert_eq!(
        native_try_mirror_answer(Some(m), || panic!("switch read on the cached path")),
        NativeTryMirror::Mirror(m)
    );
}

/// A refusal declines (the caller collects) with the switch on, and takes
/// the old infallible door with it off.
#[test]
fn gci_i22oo_a_refusal_declines_only_under_the_switch() {
    assert_eq!(native_try_mirror_answer(None, || true), NativeTryMirror::Decline);
    assert_eq!(native_try_mirror_answer(None, || false), NativeTryMirror::Infallible);
}


/// The door keeps ZGC's two arms ahead of the new rule, and the one
/// caller collects between its two asks (the precondition: `this` is
/// dead by then, which the native's comment states).
#[test]
fn gci_i22oo_the_door_and_its_caller_keep_their_shape() {
    let src = concat!(include_str!("../vm_exec/mod.rs"), "\n", include_str!("../vm_exec/method.rs"), "\n", include_str!("../vm_exec/report_pad_access.rs"), "\n", include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../vm_exec/load_class.rs"), "\n", include_str!("../vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../vm_exec/new_object.rs"), "\n", include_str!("../vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../vm_exec/invoke_special_shared.rs"), "\n", include_str!("../vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../vm_exec/jni_table_native_call.rs")).replace("\r\n", "\n");
    let body = src
        .split("    fn try_get_class_mirror(&mut self, class_id: ClassId) -> Option<ObjectRef> \u{7b}")
        .nth(1)
        .and_then(|rest| rest.split("\n    fn ").next())
        .expect("try_get_class_mirror body");
    let zgc = body
        .find("zgc_internal_alloc_converted(self.shared)")
        .expect("the ZGC arm");
    let zgc_off = body
        .find("heap_is_zgc(self.shared)")
        .expect("the ZGC kill arm");
    let rule = body
        .find("native_try_mirror_answer(fallible, native_mirror_reclaim_enabled)")
        .expect("the rule");
    assert!(zgc < zgc_off && zgc_off < rule);
    let native = include_str!("../../../../native-builtins/src/lib_parts/arrays_range_or_throw.rs")
        .replace("\r\n", "\n");
    let get_class = native
        .split("fn native_object_get_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult \u{7b}")
        .nth(1)
        .and_then(|rest| rest.split("\n\u{7d}\n").next())
        .expect("native_object_get_class body");
    let first = get_class
        .find("ctx.try_get_class_mirror(display_id)")
        .expect("the first ask");
    let reclaim = get_class
        .find("ctx.reclaim_before_alloc_retry()")
        .expect("the collection");
    let second = get_class[reclaim..]
        .find("ctx.try_get_class_mirror(display_id)")
        .expect("the second ask");
    assert!(first < reclaim && second > 0);
}
