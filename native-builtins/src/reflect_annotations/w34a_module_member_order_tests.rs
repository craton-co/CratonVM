use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

struct Teardown(usize);
impl Drop for Teardown {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(self.0);
    }
}

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the mock and the lock-key registry
    // only hash and compare the address.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

#[test]
fn w34a_d_the_first_key_orders_as_java_string_compare_to() {
    assert_eq!(
        module_member_java_string_compare("java.base", "java.base"),
        0
    );
    assert_eq!(module_member_java_string_compare("a", "b"), -1);
    assert_eq!(
        module_member_java_string_compare("java.sql", "java.se"),
        'q' as i32 - 'e' as i32
    );
    assert_eq!(
        module_member_java_string_compare("java.xml", "java.xml.crypto"),
        -7
    );
    // UTF-16 units, not scalar values: U+FF61 sorts after the surrogate
    // pair of U+1F600 in Java (0xFF61 > 0xD83D).
    assert!(module_member_java_string_compare("\u{FF61}", "\u{1F600}") > 0);
    assert_eq!(module_member_key_order(None, Some("x")), -1);
    assert_eq!(module_member_key_order(Some("x"), None), 1);
}

/// Two distinct stand-ins with ONE identity hash and a tied first key:
/// the old `compareTo` answered 0 and a `TreeSet` kept one of them. The
/// tie-break is never 0, antisymmetric, and the same on every call.
#[test]
fn w34a_d_same_hash_stand_ins_never_compare_equal() {
    const VM: usize = 0x34A0_D001;
    let _t = Teardown(VM);
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let (a, b) = (at(0x1_34A0_D100), at(0x2_34A0_D100));
    assert_eq!(
        ctx.identity_hash_code(a),
        ctx.identity_hash_code(b),
        "premise"
    );
    let ab = module_member_tie_break(&ctx, a, b);
    let ba = module_member_tie_break(&ctx, b, a);
    assert_ne!(ab, 0, "two objects are never equal");
    assert_eq!(ab, -ba, "antisymmetric");
    assert_eq!(module_member_tie_break(&ctx, a, b), ab, "stable");
    assert_eq!(module_member_tie_break(&ctx, a, a), 0);
}

/// The whole native on the mock (no `compareTo` bytecode there): the
/// same stand-in is 0, two stand-ins are ordered and antisymmetric, and
/// a null argument throws as the JDK's `compareTo` does.
#[test]
fn w34a_d_the_native_orders_distinct_stand_ins() {
    const VM: usize = 0x34A0_D002;
    let _t = Teardown(VM);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let a = ctx.fresh_object_ref();
    let b = ctx.fresh_object_ref();
    let same = native_module_exports_compare_to(
        &mut ctx,
        &[Value::Object(Some(a)), Value::Object(Some(a))],
    )
    .unwrap();
    assert_eq!(same, Some(Value::Int(0)));
    let ab = native_module_exports_compare_to(
        &mut ctx,
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap();
    let ba = native_module_exports_compare_to(
        &mut ctx,
        &[Value::Object(Some(b)), Value::Object(Some(a))],
    )
    .unwrap();
    match (ab, ba) {
        (Some(Value::Int(x)), Some(Value::Int(y))) => {
            assert_ne!(x, 0);
            assert_eq!(x.signum(), -y.signum());
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(native_module_exports_compare_to(
        &mut ctx,
        &[Value::Object(Some(a)), Value::Object(None)],
    )
    .is_err());
}
