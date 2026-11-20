use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0x12D_1A00;
const VM_B: usize = 0x12D_1B00;

/// Drops both VMs' loader rows even when an assertion fails.
struct Teardown;
impl Drop for Teardown {
    fn drop(&mut self) {
        for vm in [VM_A, VM_B] {
            crate::classloader::forget_vm_loader_singletons(vm);
        }
    }
}

fn vm_ctx(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

/// `URLClassLoader.close()` through the real-JDK bridge. The mock answers
/// `getURLs` with null, so no classpath root is retracted.
fn real_close(ctx: &mut MockNativeContext, loader: ObjectRef) {
    let mut r = NativeMethodRegistry::new();
    crate::servlet::register_url_classloader_close_bridge(&mut r);
    let close = r
        .find("java/net/URLClassLoader", "close", "()V")
        .expect("the real-JDK close() bridge is registered");
    assert!(close(ctx, &[Value::Object(Some(loader))]).is_ok());
}

#[test]
fn real_mode_close_is_per_vm_dropped_at_teardown_and_pruned() {
    let _t = Teardown;
    let mut ctx_a = vm_ctx(VM_A);
    let mut ctx_b = vm_ctx(VM_B);
    // Two fresh mocks normally number their objects alike, so the two
    // loaders share an identity hash: the collision two real heaps
    // produce, and the case the old bare-hash set got wrong. The
    // assertions hold either way.
    let loader_a = ctx_a.fresh_object_ref();
    let loader_b = ctx_b.fresh_object_ref();

    real_close(&mut ctx_a, loader_a);
    assert!(crate::classloader::ucl_is_closed(&ctx_a, loader_a));
    assert!(
        crate::classloader::ucl_is_closed(&ctx_a, loader_a),
        "one closed set serves the synthetic and the real-mode readers"
    );
    assert!(
        !crate::classloader::ucl_is_closed(&ctx_b, loader_b),
        "A's close() must not close B's loader, even on a shared identity hash"
    );

    // B closes its own loader; A's teardown drops only A's row.
    real_close(&mut ctx_b, loader_b);
    crate::classloader::forget_vm_loader_singletons(VM_A);
    assert!(
        !crate::classloader::ucl_is_closed(&ctx_a, loader_a),
        "A's teardown drops A's row"
    );
    assert!(
        crate::classloader::ucl_is_closed(&ctx_b, loader_b),
        "and keeps B's"
    );

    // B's loader dies: B's reconcile prunes the row with it.
    let dead = loader_b.as_ptr() as usize;
    let is_marked = move |addr: usize| addr != dead;
    let _ = crate::classloader::gc_reconcile_defining_loaders(
        crate::classloader::detached_loader_tables(VM_B),
        &is_marked,
        &cratonvm_types::PointerMap::default(),
        None,
    );
    assert!(
        !crate::classloader::ucl_is_closed(&ctx_b, loader_b),
        "a dead loader's closed row goes with it"
    );
}
