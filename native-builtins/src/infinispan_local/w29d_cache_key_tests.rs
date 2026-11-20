use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeInvokeAccess};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only, 8-byte aligned, never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

#[test]
fn identity_keys_are_per_object_and_per_vm() {
    const VM: usize = 0x29D1_5A01;
    const OTHER_VM: usize = 0x29D1_5A02;
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let other = crate::test_utils::mock_ctx();
    other.set_vm_identity(OTHER_VM);
    let (a, b) = (obj(0x1_29D1_5A00), obj(0x2_29D1_5A00));
    assert_eq!(
        ctx.identity_hash_code(a),
        ctx.identity_hash_code(b),
        "premise: the two key objects share an identity hash"
    );
    let ka = CacheKey::from_identity(&ctx, a);
    assert_eq!(
        ka,
        CacheKey::from_identity(&ctx, a),
        "stable for one object"
    );
    assert_ne!(
        ka,
        CacheKey::from_identity(&ctx, b),
        "a same-hash object is another key"
    );
    assert_ne!(
        ka,
        CacheKey::from_identity(&other, a),
        "another VM's object is another key"
    );
    assert_eq!(ka.as_bytes().len(), 17);
    assert_eq!(
        ka.as_bytes()[0],
        0xFF,
        "never a UTF-8 String key's first byte"
    );
    crate::forget_vm_lock_keys(VM);
    crate::forget_vm_lock_keys(OTHER_VM);
}
