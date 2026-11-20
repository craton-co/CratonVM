use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const W21A_VM: usize = 0x0A22_1008;

struct Teardown;
impl Drop for Teardown {
    fn drop(&mut self) {
        forget_vm_async_future_roots(W21A_VM);
    }
}

#[test]
fn w21a_completed_futures_are_released_when_the_list_doubles() {
    let _teardown = Teardown;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(W21A_VM);
    // `FutureTask.state` at slot 0 of the mock's class 0.
    ctx.set_declared_fields(
        cratonvm_types::ClassId::new(0),
        vec![FieldMetadata {
            name: "state".to_string(),
            descriptor: "I".to_string(),
            access_flags: 0,
            slot_index: 0,
            declaring_class_id: cratonvm_types::ClassId::new(0),
            is_static: false,
        }],
    );
    let base = ctx.global_root_count();
    // Every fourth task is still NEW (0); the rest completed NORMAL (2).
    let mut pending = 0usize;
    for i in 0..ASYNC_FUTURE_PRUNE_MIN - 1 {
        let future = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
        let state = if i % 4 == 0 {
            pending += 1;
            0
        } else {
            2
        };
        ctx.set_field(future, 0, Value::Int(state));
        track_async_future(&mut ctx, future);
    }
    assert_eq!(
        ctx.global_root_count(),
        base + ASYNC_FUTURE_PRUNE_MIN - 1,
        "below the threshold nothing is pruned"
    );
    let last = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    ctx.set_field(last, 0, Value::Int(0));
    pending += 1;
    track_async_future(&mut ctx, last);
    assert_eq!(
        ctx.global_root_count(),
        base + pending,
        "the completed tasks' roots are released at the threshold"
    );
    assert_eq!(
        ASYNC_FUTURE_ROOTS.peek(W21A_VM, |roots| roots.len()),
        Some(pending),
        "only the pending tasks stay tracked"
    );
}
