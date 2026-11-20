use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities: no other test uses them.
const W22A_VM_A: usize = 0x0A22_7A08;
const W22A_VM_B: usize = 0x0A22_7B08;

struct Forget;
impl Drop for Forget {
    fn drop(&mut self) {
        forget_vm_xnio_conduit_rows(W22A_VM_A);
        forget_vm_xnio_conduit_rows(W22A_VM_B);
    }
}

/// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
/// item 6): re-binding a conduit's ready handler releases the root it
/// displaced (it used to leak one per re-registration); a VM's teardown
/// drops its handler / IO-thread / source rows and the channels they
/// named, and leaves another VM's rows alone.
#[test]
fn w22a_conduit_rows_release_displaced_roots_and_go_with_their_vm() {
    let _forget = Forget;
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(W22A_VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(W22A_VM_B);

    let channel_a = a.fresh_object_ref();
    let handler_a1 = a.fresh_object_ref();
    let handler_a2 = a.fresh_object_ref();
    remember_ready_handler(read_ready_handler_registry(), &mut a, channel_a, handler_a1);
    assert_eq!(a.global_root_count(), 1);
    remember_ready_handler(read_ready_handler_registry(), &mut a, channel_a, handler_a2);
    assert_eq!(
        a.global_root_count(),
        1,
        "the displaced handler's root is released"
    );
    assert_eq!(
        ready_handler_of(read_ready_handler_registry(), &a, channel_a),
        Some(handler_a2)
    );

    let io_thread_a = a.fresh_object_ref();
    remember_source_io_thread(&mut a, channel_a, io_thread_a);
    remember_source_io_thread(&mut a, channel_a, io_thread_a);
    assert_eq!(a.global_root_count(), 2, "one IO-thread root, not two");

    let source_a = a.fresh_object_ref();
    let source_id = register_source_channel(ConduitTransport::Pipe(Arc::new(Pipe::new(16))));
    remember_source_obj(&mut a, source_a, source_id);
    assert!(get_source_channel(source_id).is_some());

    let channel_b = b.fresh_object_ref();
    let handler_b = b.fresh_object_ref();
    remember_ready_handler(read_ready_handler_registry(), &mut b, channel_b, handler_b);

    forget_vm_xnio_conduit_rows(W22A_VM_A);
    assert_eq!(
        ready_handler_of(read_ready_handler_registry(), &a, channel_a),
        None
    );
    assert!(!channel_io_thread_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .any(|k| k.vm == W22A_VM_A));
    assert!(source_obj_handles_by_id(&a).is_empty());
    assert!(
        get_source_channel(source_id).is_none(),
        "A's channel goes with A"
    );
    assert_eq!(
        ready_handler_of(read_ready_handler_registry(), &b, channel_b),
        Some(handler_b),
        "B's rows survive A's teardown"
    );
}
