use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

fn w17f_for_each_remaining_moves_the_collector(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "forEachRemaining" {
        return None;
    }
    // `invoke_virtual` prepends the receiver itself: args[0] is the consumer.
    let old = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let storage = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
    for i in 0..2 {
        let element = ctx.fresh_object_ref();
        ctx.set_array_element(storage, i, Value::Object(Some(element)));
    }
    let moved = ctx.fresh_object_ref();
    ctx.set_field(moved, 0, Value::Object(Some(storage)));
    ctx.set_field(moved, 1, Value::Int(2));
    ctx.remap_native_pin_addr_for_test(old.as_ptr() as usize, moved.as_ptr() as usize);
    Some(Ok(None))
}

#[test]
fn w17f_drain_reads_the_collector_at_its_current_address() {
    let mut ctx = MockNativeContext::new();
    let spliterator = ctx.fresh_object_ref();
    ctx.set_invoke_virtual_hook(w17f_for_each_remaining_moves_the_collector);

    let out = drain_spliterator(&mut ctx, spliterator).expect("the drain succeeds");

    assert_eq!(
        ctx.array_length(out),
        2,
        "both drained elements must be read off the collector's current copy"
    );
    for i in 0..2 {
        assert!(matches!(
            ctx.get_array_element(out, i),
            Value::Object(Some(_))
        ));
    }
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
