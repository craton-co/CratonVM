use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_async_future_roots(vm);
        }
    }
}

fn mock_on(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

/// Another VM's `awaitQuiescence` neither sees nor releases this VM's
/// handles (a handle indexes ITS VM's root table); this VM's own call
/// releases a future once it is done.
#[test]
fn w19b_async_future_roots_are_per_vm() {
    const VM_A: usize = 0x19B0_0C01;
    const VM_B: usize = 0x19B0_0C02;
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut a = mock_on(VM_A);
    let mut b = mock_on(VM_B);
    let future = a.alloc_object(cratonvm_types::ClassId::new(0), 1);
    // VM B holds a root with the same handle number VM A is about to mint.
    let b_obj = b.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let b_root = b.add_global_root(b_obj);
    let base_a = a.global_root_count();

    track_async_future(&mut a, future);
    assert_eq!(a.global_root_count(), base_a + 1);

    // VM B: nothing of its own is tracked, so it is quiescent, and a
    // "done" answer for whatever it might resolve releases nothing.
    b.set_invoke_virtual_result(Ok(Some(Value::Int(1))));
    assert!(async_tasks_quiescent(&mut b));
    assert_eq!(
        b.resolve_global_root(b_root),
        Some(b_obj),
        "VM B's own root survives VM A's tracking"
    );

    // VM A: `isDone()` unknown -> pending, root kept.
    assert!(!async_tasks_quiescent(&mut a));
    assert_eq!(a.global_root_count(), base_a + 1);
    // `isDone()` true -> quiescent, root released.
    a.set_invoke_virtual_result(Ok(Some(Value::Int(1))));
    assert!(async_tasks_quiescent(&mut a));
    assert_eq!(a.global_root_count(), base_a);
    assert!(async_tasks_quiescent(&mut a), "nothing left to wait for");
}

/// The constructor helper stores a fresh array and answers the receiver.
#[test]
fn w19b_store_fresh_array_fills_the_receiver() {
    let mut ctx = mock_on(0x19B0_0C03);
    let queue = ctx.alloc_object(cratonvm_types::ClassId::new(0), 3);
    let this = uce_store_fresh_ref_array(&mut ctx, queue, 16);
    assert_eq!(this, queue);
    match ctx.get_field(this, 0) {
        Value::Object(Some(arr)) => assert_eq!(ctx.array_length(arr), 16),
        other => panic!("expected the backing array, got {other:?}"),
    }
    let ints =
        uce_store_fresh_array(&mut ctx, queue, 1, cratonvm_types::ArrayElementType::Int, 4);
    match ctx.get_field(ints, 1) {
        Value::Object(Some(arr)) => assert_eq!(ctx.array_length(arr), 4),
        other => panic!("expected the int array, got {other:?}"),
    }
}
