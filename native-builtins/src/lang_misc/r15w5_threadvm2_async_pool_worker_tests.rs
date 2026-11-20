use super::is_vm_async_pool_worker;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{FieldMetadata, NativeHeapAccess};
use cratonvm_types::Value;

// Private VM identities: no other test uses them.
const VM_A: usize = 0x15_5A_2001;
const VM_B: usize = 0x15_5A_2002;

#[test]
fn only_a_worker_of_this_vms_async_pool_is_one() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM_A);
    ctx.set_absent_field_answers_null(true);
    let tpe = ctx.declare_loaded_class("java/util/concurrent/ThreadPoolExecutor");
    let worker_class =
        ctx.declare_loaded_class("java/util/concurrent/ThreadPoolExecutor$Worker");
    ctx.set_declared_fields(
        worker_class,
        vec![FieldMetadata {
            name: "this$0".to_string(),
            descriptor: "Ljava/util/concurrent/ThreadPoolExecutor;".to_string(),
            access_flags: 0x1010,
            slot_index: 0,
            declaring_class_id: worker_class,
            is_static: false,
        }],
    );
    let pool = ctx.alloc_object(tpe, 4);
    let user_pool = ctx.alloc_object(tpe, 4);
    let async_worker = ctx.alloc_object(worker_class, 2);
    let user_worker = ctx.alloc_object(worker_class, 2);
    ctx.set_field(async_worker, 0, Value::Object(Some(pool)));
    ctx.set_field(user_worker, 0, Value::Object(Some(user_pool)));
    // No async pool yet: nothing is its worker.
    assert!(!is_vm_async_pool_worker(&mut ctx, async_worker));
    // An unregistered handle: the cached ref answers.
    crate::ASYNC_POOL.with(VM_A, |slot| *slot = Some((usize::MAX, pool)));
    assert!(is_vm_async_pool_worker(&mut ctx, async_worker));
    assert!(!is_vm_async_pool_worker(&mut ctx, user_worker));
    // Another VM's pool is not this VM's.
    ctx.set_vm_identity(VM_B);
    assert!(!is_vm_async_pool_worker(&mut ctx, async_worker));
    crate::forget_vm_async_worker_pool(VM_A);
    crate::forget_vm_async_worker_pool(VM_B);
}
