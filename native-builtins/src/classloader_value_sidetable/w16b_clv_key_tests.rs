use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key / stored value only; nothing on these paths
    // dereferences it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn get(
    ctx: &mut crate::test_utils::MockNativeContext,
    clv: ObjectRef,
    cl: Option<ObjectRef>,
) -> Option<ObjectRef> {
    match native_aclv_get(ctx, &[Value::Object(Some(clv)), Value::Object(cl)]) {
        Ok(Some(Value::Object(o))) => o,
        _ => panic!("AbstractClassLoaderValue.get answered a non-object"),
    }
}

/// `putIfAbsent`; answers the value it found already present, if any.
fn put(
    ctx: &mut crate::test_utils::MockNativeContext,
    clv: ObjectRef,
    cl: Option<ObjectRef>,
    value: ObjectRef,
) -> Option<ObjectRef> {
    match native_aclv_put_if_absent(
        ctx,
        &[
            Value::Object(Some(clv)),
            Value::Object(cl),
            Value::Object(Some(value)),
        ],
    ) {
        Ok(Some(Value::Object(o))) => o,
        _ => panic!("AbstractClassLoaderValue.putIfAbsent answered a non-object"),
    }
}

/// The page's retire test: VM B's loader and CLV carry the identity hashes
/// of VM A's pair. B used to be handed A's key and read A's row -- a raw
/// address in A's heap. Now each VM sees only its own rows, for a real
/// loader and for the bootstrap (null) loader alike.
#[test]
fn two_vms_with_one_identity_hash_pair_keep_distinct_rows() {
    const VM_A: usize = 0xB160_0001;
    const VM_B: usize = 0xB160_0002;
    let mut a = ctx_for(VM_A);
    let mut b = ctx_for(VM_B);
    let (cl_a, clv_a, val_a) = (obj(0x1_B16A_0010), obj(0x1_B16A_0020), obj(0x1_B16A_0030));
    let (cl_b, clv_b, val_b) = (obj(0x2_B16A_0010), obj(0x2_B16A_0020), obj(0x2_B16A_0030));
    assert_eq!(a.identity_hash_code(cl_a), b.identity_hash_code(cl_b));
    assert_eq!(a.identity_hash_code(clv_a), b.identity_hash_code(clv_b));

    assert_eq!(put(&mut a, clv_a, Some(cl_a), val_a), None);
    assert_eq!(put(&mut a, clv_a, None, val_a), None);
    assert_eq!(get(&mut b, clv_b, Some(cl_b)), None, "B read A's row");
    assert_eq!(get(&mut b, clv_b, None), None, "B read A's bootstrap row");
    assert_eq!(put(&mut b, clv_b, Some(cl_b), val_b), None);
    assert_eq!(get(&mut a, clv_a, Some(cl_a)), Some(val_a));
    assert_eq!(get(&mut b, clv_b, Some(cl_b)), Some(val_b));

    // Teardown of A takes A's rows, nothing of B's.
    crate::forget_vm_lock_keys(VM_A);
    assert_eq!(get(&mut b, clv_b, Some(cl_b)), Some(val_b));
    assert_eq!(get(&mut a, clv_a, Some(cl_a)), None);
    assert_eq!(get(&mut a, clv_a, None), None);
    crate::forget_vm_lock_keys(VM_A);
    crate::forget_vm_lock_keys(VM_B);
}

/// A row goes when EITHER of its two objects dies, and a later object
/// with the dead one's identity hash inherits nothing (the old registry
/// rebound the dead slot to it).
#[test]
fn a_row_goes_when_its_loader_or_its_value_key_dies() {
    const VM: usize = 0xB160_0003;
    let mut c = ctx_for(VM);
    let (cl, clv, clv2, val) = (
        obj(0x1_B16B_0010),
        obj(0x1_B16B_0020),
        obj(0x1_B16B_0040),
        obj(0x1_B16B_0030),
    );
    put(&mut c, clv, Some(cl), val);
    put(&mut c, clv2, Some(cl), val);
    put(&mut c, clv2, None, val);
    // The loader dies: both of its rows go, the bootstrap row stays.
    let loader = cl.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|a| a != loader);
    assert_eq!(get(&mut c, clv, Some(cl)), None);
    assert_eq!(get(&mut c, clv2, Some(cl)), None);
    assert_eq!(get(&mut c, clv2, None), Some(val));
    // A new loader with the dead one's hash starts empty.
    let reborn = obj(0x3_B16B_0010);
    assert_eq!(get(&mut c, clv2, Some(reborn)), None);
    // The CLV dies: its bootstrap row goes too, and a new CLV with its
    // hash starts empty.
    let dead_clv = clv2.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|a| a != dead_clv);
    assert_eq!(get(&mut c, obj(0x3_B16B_0040), None), None);
    crate::forget_vm_lock_keys(VM);
}

/// Both objects moved by a collection keep their row: the epilogue
/// re-addresses their slots, and a later sweep judges them there.
#[test]
fn a_moved_loader_and_value_keep_their_row() {
    const VM: usize = 0xB160_0004;
    let mut c = ctx_for(VM);
    let (cl_old, cl_new) = (0x1_B16C_0010usize, 0x3_B16C_0010usize);
    let (clv_old, clv_new) = (0x1_B16C_0020usize, 0x3_B16C_0020usize);
    let val = obj(0x1_B16C_0030);
    put(&mut c, obj(clv_old), Some(obj(cl_old)), val);
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(cl_old, cl_new);
    moved.insert(clv_old, clv_new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == cl_old || a == clv_old, &moved);
    assert_eq!(get(&mut c, obj(clv_new), Some(obj(cl_new))), Some(val));
    crate::gc_sweep_lock_keys(VM, &|a| a == cl_new || a == clv_new);
    assert_eq!(get(&mut c, obj(clv_new), Some(obj(cl_new))), Some(val));
    crate::forget_vm_lock_keys(VM);
}
