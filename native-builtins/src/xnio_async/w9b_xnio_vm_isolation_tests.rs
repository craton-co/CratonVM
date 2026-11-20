use super::*;
use cratonvm_native_api::NativeHeapAccess as _;

const VM_A: usize = 0x9B00_0501;
const VM_B: usize = 0x9B00_0502;

/// Two VMs' objects with the same identity hash. The mock's identity hash
/// is the address truncated to `i32`, so two addresses 4 GiB apart share
/// one -- the "lone occupant moved" shape the rebind rule used to take for
/// a GC relocation and answer with VM A's key. Never dereferenced.
#[test]
fn a_second_vm_never_inherits_an_object_key() {
    let a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    let in_a = unsafe { ObjectRef::from_raw(0x1_9B00_1000usize as *mut u8) };
    let in_b = unsafe { ObjectRef::from_raw(0x2_9B00_1000usize as *mut u8) };
    assert_eq!(a.identity_hash_code(in_a), b.identity_hash_code(in_b));

    let ka = xnio_obj_key_for(&a, in_a);
    let kb = xnio_obj_key_for(&b, in_b);
    assert_ne!(ka, kb, "VM B's object must not inherit VM A's key");
    assert_eq!(xnio_obj_key_for(&a, in_a), ka, "A's key is stable");

    // A genuine relocation inside ONE VM still keeps its key. Since
    // gc-common w17-b the key lives in the lib.rs lock-key registry, which
    // learns of the move from the stop-the-world remap, not by rebinding
    // the slot to whatever address the next caller passes.
    let moved_addr = 0x3_9B00_1000usize;
    let moved = unsafe { ObjectRef::from_raw(moved_addr as *mut u8) };
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(in_a.as_ptr() as usize, moved_addr);
    crate::gc_sweep_and_remap_lock_keys(VM_A, &|_| true, &pm);
    assert_eq!(xnio_obj_key_for(&a, moved), ka);

    // A handle no `register_*` ever mints (the counter starts at 1 and
    // only grows): since gc-common w17-b the hook also drops the store
    // row a freed key's handle names, and a small positive literal could
    // be a parallel test's live handle.
    registries().map_handles_by_obj.lock().insert(ka, -0x9B05);
    crate::forget_vm_lock_keys(VM_A);
    forget_vm_xnio_roots(VM_A);
    assert!(!registries().map_handles_by_obj.lock().contains_key(&ka));
    // A key is never re-minted after its VM's teardown.
    let a2 = crate::test_utils::mock_ctx();
    a2.set_vm_identity(VM_A + 16);
    assert_ne!(xnio_obj_key_for(&a2, in_a), ka);
    crate::forget_vm_lock_keys(VM_A + 16);
    forget_vm_xnio_roots(VM_A + 16);
    crate::forget_vm_lock_keys(VM_B);
    forget_vm_xnio_roots(VM_B);
}

#[test]
fn options_store_and_fill_latch_are_per_vm() {
    const VM_C: usize = 0x9B00_0503;
    const VM_D: usize = 0x9B00_0504;
    // SAFETY: a map value only, never dereferenced.
    let opt = unsafe { ObjectRef::from_raw(0x9B00_2000usize as *mut u8) };
    OPTIONS_STORE.with(VM_C, |s| {
        s.insert("WORKER_IO_THREADS".to_string(), (1, opt));
    });
    OPTIONS_FILLED.with(VM_C, |d| *d = true);
    assert_eq!(OPTIONS_FILLED.peek(VM_D, |d| *d), None);
    assert_eq!(
        OPTIONS_STORE.peek(VM_D, |s| s.contains_key("WORKER_IO_THREADS")),
        None,
        "VM D must run its own Options fill, not read VM C's objects"
    );
    forget_vm_xnio_roots(VM_C);
    assert!(!OPTIONS_STORE.has_row(VM_C));
    assert!(!OPTIONS_FILLED.has_row(VM_C));
}
