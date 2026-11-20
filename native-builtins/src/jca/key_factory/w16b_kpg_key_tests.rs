use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; nothing on these paths dereferences it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn fill(ctx: &dyn NativeContext, o: ObjectRef, algo: i32, bits: i32, name: &str, bc: bool) {
    set_kpg_algo(ctx, o, algo);
    set_kpg_keysize(ctx, o, bits);
    set_kpg_name(ctx, o, name.to_string());
    set_kpg_bcprov(ctx, o, bc);
    set_kf_algo(ctx, o, algo);
}

/// The page's retire test for the `kpg` copy: two VMs' generators with one
/// identity hash keep distinct rows, and one VM's teardown takes only its
/// own.
#[test]
fn two_vms_generators_with_one_identity_hash_keep_distinct_rows() {
    const VM_A: usize = 0xB161_0001;
    const VM_B: usize = 0xB161_0002;
    let a = ctx_for(VM_A);
    let b = ctx_for(VM_B);
    let (ga, gb) = (obj(0x1_B16D_0010), obj(0x2_B16D_0010));
    assert_eq!(a.identity_hash_code(ga), b.identity_hash_code(gb));
    fill(&a, ga, ALGO_RSA, 3072, "RSA", false);
    assert_eq!(get_kpg_algo(&b, gb), None, "B read A's algorithm");
    fill(&b, gb, ALGO_EC, 384, "EC", true);
    assert_ne!(kpg_obj_key_for(&a, ga), kpg_obj_key_for(&b, gb));
    assert_eq!(get_kpg_algo(&a, ga), Some(ALGO_RSA));
    assert_eq!(get_kpg_keysize(&a, ga), Some(3072));
    assert_eq!(get_kpg_name(&a, ga).as_deref(), Some("RSA"));
    assert!(!get_kpg_bcprov(&a, ga));
    assert_eq!(get_kf_algo(&a, ga), Some(ALGO_RSA));
    assert_eq!(get_kpg_algo(&b, gb), Some(ALGO_EC));
    assert!(get_kpg_bcprov(&b, gb));

    crate::forget_vm_lock_keys(VM_A);
    assert_eq!(get_kpg_algo(&b, gb), Some(ALGO_EC));
    assert_eq!(get_kpg_name(&b, gb).as_deref(), Some("EC"));
    assert_eq!(get_kpg_algo(&a, ga), None);
    assert_eq!(get_kpg_keysize(&a, ga), None);
    crate::forget_vm_lock_keys(VM_A);
    crate::forget_vm_lock_keys(VM_B);
}

/// A dead generator's rows go with its key, in all five tables, and a
/// later generator with its identity hash inherits none of them (the old
/// registry rebound the dead slot to it).
#[test]
fn a_dead_generators_rows_go_with_its_key() {
    const VM: usize = 0xB161_0003;
    let c = ctx_for(VM);
    let dead = obj(0x1_B16E_0010);
    fill(&c, dead, ALGO_RSA, 4096, "RSA", true);
    let kd = kpg_obj_key_for(&c, dead);
    let addr = dead.as_ptr() as usize;
    assert!(crate::gc_sweep_lock_keys(VM, &|a| a != addr) >= 1);
    assert!(!kpg_algo_table().lock().contains_key(&kd));
    assert!(!kpg_keysize_table().lock().contains_key(&kd));
    assert!(!kf_algo_table().lock().contains_key(&kd));
    assert!(!kpg_name_table().lock().contains_key(&kd));
    assert!(!kpg_bcprov_table().lock().contains_key(&kd));

    let reborn = obj(0x2_B16E_0010);
    assert_ne!(kpg_obj_key_for(&c, reborn), kd);
    assert_eq!(get_kpg_algo(&c, reborn), None);
    assert_eq!(get_kpg_keysize(&c, reborn), None);
    assert!(!get_kpg_bcprov(&c, reborn));
    crate::forget_vm_lock_keys(VM);
}

/// A live generator that a collection moves keeps its rows.
#[test]
fn a_moved_generator_keeps_its_rows() {
    const VM: usize = 0xB161_0004;
    let c = ctx_for(VM);
    let (old, new) = (0x1_B16F_0010usize, 0x3_B16F_0010usize);
    fill(&c, obj(old), ALGO_EC, 256, "EC", false);
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old, &moved);
    assert_eq!(get_kpg_algo(&c, obj(new)), Some(ALGO_EC));
    crate::gc_sweep_lock_keys(VM, &|a| a == new);
    assert_eq!(get_kpg_keysize(&c, obj(new)), Some(256));
    crate::forget_vm_lock_keys(VM);
}

thread_local! {
    /// (old, new) address of the key the `getName` hook "moves".
    static W16B_MOVE: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    /// The receiver `getEncoded` was invoked on.
    static W16B_ENCODED_ON: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `Class.getName()` "collects" (every pin on the key's old address now
/// reads the new one, as a moving collection leaves it) and answers
/// `X509EncodedKeySpec`; `getEncoded` records its receiver.
fn w16b_move_key_during_get_name(
    ctx: &mut crate::test_utils::MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    match method_name {
        "getName" => {
            let (old, new) = W16B_MOVE.with(|m| m.get());
            ctx.remap_native_pin_addr_for_test(old, new);
            let s = ctx.create_string("java.security.spec.X509EncodedKeySpec");
            Some(Ok(Some(Value::Object(Some(s)))))
        }
        "getEncoded" => {
            W16B_ENCODED_ON.with(|c| c.set(receiver.as_ptr() as usize));
            let der = ctx.new_array(ArrayElementType::Byte, 2);
            Some(Ok(Some(Value::Object(Some(der)))))
        }
        _ => None,
    }
}

/// The third audit site: `getKeySpec(key, X509EncodedKeySpec.class)`
/// invoked `getEncoded` on the key's pre-`getName` address.
#[test]
fn get_key_spec_reads_the_key_after_get_name_moves_it() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let key = ctx.fresh_object_ref();
    let moved = ctx.fresh_object_ref();
    let spec_class = ctx.fresh_object_ref();
    W16B_MOVE.with(|m| m.set((key.as_ptr() as usize, moved.as_ptr() as usize)));
    W16B_ENCODED_ON.with(|c| c.set(0));
    ctx.set_invoke_virtual_hook(w16b_move_key_during_get_name);
    // A null receiver skips the provider-delegate arm; the spec object
    // `new_object_initialized` builds is not what this test is about.
    let _ = kf_get_key_spec(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(key)),
            Value::Object(Some(spec_class)),
        ],
    );
    assert_eq!(
        W16B_ENCODED_ON.with(|c| c.get()),
        moved.as_ptr() as usize,
        "getEncoded ran on the key's pre-move address"
    );
}
