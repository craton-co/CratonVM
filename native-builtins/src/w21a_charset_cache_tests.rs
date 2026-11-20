use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities: no other test publishes under them.
const W21A_VM_A: usize = 0x0A21_8008;
const W21A_VM_B: usize = 0x0A21_9008;
const W21A_NAME: &str = "w21a-test-charset";

#[test]
fn w21a_charset_rows_are_per_vm_and_forgotten_with_the_vm() {
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(W21A_VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(W21A_VM_B);
    let cs_a = a.fresh_object_ref();
    // B's global-ref slot 1 holds an unrelated object: the same index VM
    // A's handle gets in A's table.
    let unrelated_b = b.fresh_object_ref();
    let unrelated_handle = b.add_global_root(unrelated_b);

    publish_real_charset(&mut a, W21A_NAME.to_string(), cs_a);
    assert_eq!(cached_real_charset(&a, W21A_NAME), Some(cs_a));
    assert_eq!(
        cached_real_charset(&b, W21A_NAME),
        None,
        "VM B must not resolve VM A's handle in its own table"
    );

    // Two racing publishes in B: the loser's root is released in B's table.
    let cs_b1 = b.fresh_object_ref();
    let cs_b2 = b.fresh_object_ref();
    publish_real_charset(&mut b, W21A_NAME.to_string(), cs_b1);
    let first = REAL_CHARSET_CACHE
        .peek(W21A_VM_B, |rows| rows.get(W21A_NAME).copied())
        .flatten()
        .expect("B's row");
    publish_real_charset(&mut b, W21A_NAME.to_string(), cs_b2);
    assert_eq!(cached_real_charset(&b, W21A_NAME), Some(cs_b2));
    assert_eq!(
        b.resolve_global_root(first),
        None,
        "the replaced root is released"
    );
    assert_eq!(b.resolve_global_root(unrelated_handle), Some(unrelated_b));
    assert_eq!(cached_real_charset(&a, W21A_NAME), Some(cs_a));

    forget_vm_real_charset_cache(W21A_VM_A);
    forget_vm_real_charset_cache(W21A_VM_B);
    assert!(!REAL_CHARSET_CACHE.has_row(W21A_VM_A));
    assert_eq!(cached_real_charset(&b, W21A_NAME), None);
}

#[test]
fn w21a_system_stream_overrides_are_per_vm_and_forgotten_with_the_vm() {
    const W21A_VM_C: usize = 0x0A21_F008;
    const W21A_VM_D: usize = 0x0A22_0008;
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(W21A_VM_C);
    let mut d = crate::test_utils::mock_ctx();
    d.set_vm_identity(W21A_VM_D);
    let out_c = c.fresh_object_ref();
    // D's global-ref slot 1 holds an unrelated object: the same index VM
    // C's handle for its stream gets in C's table.
    let unrelated_d = d.fresh_object_ref();
    let unrelated_handle = d.add_global_root(unrelated_d);

    system_set_overridden_stream(&mut c, "out", out_c);
    assert_eq!(system_overridden_stream_resolved(&c, "out"), Some(out_c));
    assert_eq!(
        system_overridden_stream_resolved(&d, "out"),
        None,
        "VM D must not resolve VM C's handle in its own table"
    );

    let out_d = d.fresh_object_ref();
    system_set_overridden_stream(&mut d, "out", out_d);
    assert_eq!(
        d.resolve_global_root(unrelated_handle),
        Some(unrelated_d),
        "D's System.setOut must not release the root of D's that C's row names"
    );
    assert_eq!(system_overridden_stream_resolved(&d, "out"), Some(out_d));
    assert_eq!(system_overridden_stream_resolved(&c, "out"), Some(out_c));

    system_clear_overridden_stream(&mut d, "out");
    assert_eq!(system_overridden_stream_resolved(&d, "out"), None);
    assert_eq!(d.resolve_global_root(unrelated_handle), Some(unrelated_d));

    forget_vm_system_stream_overrides(W21A_VM_C);
    forget_vm_system_stream_overrides(W21A_VM_D);
    assert!(!SYSTEM_STREAM_OVERRIDES.has_row(W21A_VM_C));
    assert_eq!(system_overridden_stream_resolved(&c, "out"), None);
}

/// The AtomicStampedReference / AtomicMarkableReference Pair install roots
/// the receiver and the reference across the Pair's allocation, stores
/// both slots, and leaves no pin behind. (The mock never moves an object
/// at an allocation, so this checks shape and root balance.)
#[test]
fn w21a_atomic_ref_pair_install_stores_both_slots_and_releases_its_roots() {
    let mut c = crate::test_utils::mock_ctx();
    let this = c.fresh_object_ref();
    let referent = c.fresh_object_ref();
    let pins = c.native_pin_count_for_test();

    asr_install_pair(&mut c, this, Value::Object(Some(referent)), 7).expect("installed");
    assert_eq!(c.native_pin_count_for_test(), pins, "no pin left behind");
    let Value::Object(Some(pair)) = c.get_field(this, 0) else {
        panic!("the receiver's slot 0 holds the Pair");
    };
    assert_eq!(c.get_field(pair, 0), Value::Object(Some(referent)));
    assert_eq!(c.get_field(pair, 1), Value::Int(7));

    amr_install_pair(&mut c, this, Value::Object(None), true).expect("installed");
    assert_eq!(c.native_pin_count_for_test(), pins, "no pin left behind");
    let Value::Object(Some(pair2)) = c.get_field(this, 0) else {
        panic!("the receiver's slot 0 holds the new Pair");
    };
    assert_ne!(pair2, pair, "a fresh Pair is installed");
    assert_eq!(c.get_field(pair2, 0), Value::Object(None));
    assert_eq!(c.get_field(pair2, 1), Value::Int(1));
}
