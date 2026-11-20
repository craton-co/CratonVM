use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn slots(typed_reads: bool) -> TlRealSlots {
    let cid = cratonvm_types::ClassId::new;
    TlRealSlots {
        vm: 0,
        tl_cid: cid(1),
        itl_cid: cid(2),
        term_cid: cid(3),
        hash_slot: 0,
        thread_tl_slot: 1,
        thread_itl_slot: 2,
        table_slot: 0,
        entry_value_slot: 1,
        trace_vthread_locals: false,
        typed_reads,
    }
}

#[test]
fn jr21_tl2_typed_and_untyped_reads_agree() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx
        .ensure_class_initialized("java/lang/Object")
        .expect("a class id");
    let a = ctx.alloc_object(cid, 3);
    let b = ctx.alloc_object(cid, 3);
    ctx.set_field(a, 1, Value::Object(Some(b)));
    ctx.set_field(a, 2, Value::Object(None));
    for typed in [false, true] {
        let s = slots(typed);
        assert!(
            matches!(
                tl_real_ref_field(&ctx, &s, a, 1),
                Value::Object(Some(r)) if r.as_ptr() == b.as_ptr()
            ),
            "typed={typed}"
        );
        assert!(
            matches!(tl_real_ref_field(&ctx, &s, a, 2), Value::Object(None)),
            "typed={typed}"
        );
    }
}
