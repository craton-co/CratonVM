use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;

fn vm_ctx(vm: usize) -> MockNativeContext {
    let ctx = mock_ctx();
    ctx.set_vm_identity(vm);
    ctx
}

fn object(v: MethodCallResult, what: &str) -> ObjectRef {
    match v {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("{what} answered {other:?}"),
    }
}

/// Drops every lock key (and the rows filed under them) and the
/// `Security` row of the listed VMs, on every exit path.
struct Teardown(&'static [usize]);

impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            crate::forget_vm_lock_keys(vm);
            forget_vm_synthetic_security_state(vm);
        }
    }
}

/// `(backing array, size)` of a synthetic `ArrayList`.
fn list_parts(ctx: &MockNativeContext, list: ObjectRef) -> (ObjectRef, Value) {
    let Value::Object(Some(arr)) = ctx.get_field(list, 0) else {
        panic!("the list must hold its backing array");
    };
    (arr, ctx.get_field(list, 1))
}

#[test]
fn the_array_list_builders_hold_their_elements_and_release_their_roots() {
    let mut ctx = vm_ctx(0xC19_0201);
    let (a, b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());

    let elems = [Value::Object(Some(a)), Value::Object(None)];
    let list = object(pe_array_list_of(&mut ctx, &elems), "pe_array_list_of");
    let (arr, size) = list_parts(&ctx, list);
    assert_eq!(ctx.array_length(arr), 2);
    assert_eq!(ctx.get_array_element(arr, 0), Value::Object(Some(a)));
    assert_eq!(ctx.get_array_element(arr, 1), Value::Object(None));
    assert_eq!(size, Value::Int(2));
    assert_eq!(ctx.native_pin_count_for_test(), 0);

    let src = ctx.new_array(ArrayElementType::Reference, 3);
    ctx.set_array_element(src, 0, Value::Object(Some(a)));
    ctx.set_array_element(src, 2, Value::Object(Some(b)));
    let copy = object(
        pe_array_list_copy_of(&mut ctx, src),
        "pe_array_list_copy_of",
    );
    let (arr, size) = list_parts(&ctx, copy);
    assert_ne!(arr, src, "the list holds a copy, not the source array");
    assert_eq!(ctx.array_length(arr), 3);
    assert_eq!(ctx.get_array_element(arr, 0), Value::Object(Some(a)));
    assert_eq!(ctx.get_array_element(arr, 2), Value::Object(Some(b)));
    assert_eq!(size, Value::Int(3));
    assert_eq!(ctx.native_pin_count_for_test(), 0);

    let args = [Value::Int(3), Value::Object(Some(b))];
    let copies = object(native_collections_ncopies(&mut ctx, &args), "nCopies");
    let (arr, size) = list_parts(&ctx, copies);
    assert_eq!(ctx.array_length(arr), 3);
    for i in 0..3 {
        assert_eq!(ctx.get_array_element(arr, i), Value::Object(Some(b)));
    }
    assert_eq!(size, Value::Int(3));
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

/// The `StringJoiner.add` grow path on an EMPTY backing array: `cap * 2`
/// was 0 there, and the append stored past the end.
#[test]
fn a_string_joiner_grows_an_empty_backing_array_to_hold_the_element() {
    let mut ctx = vm_ctx(0xC19_0202);
    let this = ctx.fresh_object_ref();
    let list = ctx.fresh_object_ref();
    let empty = ctx.new_array(ArrayElementType::Reference, 0);
    ctx.set_field(list, 0, Value::Object(Some(empty)));
    ctx.set_field(list, 1, Value::Int(0));
    let elem = ctx.create_string("x");

    let now = sj_grow_and_append(&mut ctx, this, list, empty, 0, Value::Object(Some(elem)));
    assert_eq!(now, this);
    let (arr, size) = list_parts(&ctx, list);
    assert_ne!(arr, empty);
    assert!(ctx.array_length(arr) >= 1);
    assert_eq!(ctx.get_array_element(arr, 0), Value::Object(Some(elem)));
    assert_eq!(size, Value::Int(1));
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

#[test]
fn a_synthetic_charset_carries_its_name() {
    let mut ctx = vm_ctx(0xC19_0203);
    let cs = pe_new_charset(&mut ctx, "UTF-8").unwrap();
    let Value::Object(Some(name)) = ctx.get_field(cs, 0) else {
        panic!("the charset must carry its name");
    };
    assert_eq!(ctx.read_string(name).as_deref(), Some("UTF-8"));
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

/// A `QName`'s parts row answers only the VM that filed it, and goes when
/// the VM's lock keys are freed.
#[test]
fn qname_rows_belong_to_their_vm_and_leave_with_its_keys() {
    const VM_A: usize = 0xC19_0211;
    const VM_B: usize = 0xC19_0212;
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut registry = NativeMethodRegistry::new();
    register_qname_natives(&mut registry);
    let qn = "javax/xml/namespace/QName";
    let init = registry
        .find(
            qn,
            "<init>",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        )
        .expect("QName.<init>(String, String, String) should be registered");
    let local_part = registry
        .find(qn, "getLocalPart", "()Ljava/lang/String;")
        .expect("QName.getLocalPart should be registered");

    let mut ctx = vm_ctx(VM_A);
    let q = ctx.fresh_object_ref();
    let ns = ctx.create_string("urn:x");
    let local = ctx.create_string("item");
    let prefix = ctx.create_string("p");
    let args = [q, ns, local, prefix].map(|o| Value::Object(Some(o)));
    init(&mut ctx, &args).unwrap();
    assert_eq!(ctx.native_pin_count_for_test(), 0);
    let answer = object(local_part(&mut ctx, &args[..1]), "getLocalPart");
    assert_eq!(ctx.read_string(answer).as_deref(), Some("item"));
    let key_a = qname_row_key(&ctx, q);
    assert!(qname_parts_table().lock().unwrap().contains_key(&key_a));

    // Another VM reading an object with the same identity hash is not
    // served this VM's row.
    ctx.set_vm_identity(VM_B);
    let answer = object(local_part(&mut ctx, &args[..1]), "getLocalPart");
    assert_ne!(ctx.read_string(answer).as_deref(), Some("item"));
    assert_ne!(qname_row_key(&ctx, q), key_a);

    crate::forget_vm_lock_keys(VM_A);
    assert!(
        !qname_parts_table().lock().unwrap().contains_key(&key_a),
        "the row goes with its freed key"
    );
}

/// The `Properties` index cache: a row per (VM, object), dropped with the
/// object's lock key.
#[test]
fn properties_index_rows_belong_to_their_vm_and_leave_with_its_keys() {
    const VM_A: usize = 0xC19_0221;
    const VM_B: usize = 0xC19_0222;
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut ctx = vm_ctx(VM_A);
    let this = ctx.fresh_object_ref();
    let data = ctx.new_array(ArrayElementType::Reference, 2);
    let k = ctx.create_string("k");
    let v = ctx.create_string("v");
    ctx.set_array_element(data, 0, Value::Object(Some(k)));
    ctx.set_array_element(data, 1, Value::Object(Some(v)));

    assert_eq!(
        props_index_cache::lookup_index(&ctx, this, data, 1, "k"),
        Some(0)
    );
    assert_eq!(
        props_index_cache::lookup_index(&ctx, this, data, 1, "absent"),
        None
    );
    let key_a = crate::gc_stable_weak_lock_key(&ctx, this).unwrap();
    assert!(props_index_cache::has_row_for_test(key_a));

    ctx.set_vm_identity(VM_B);
    assert_eq!(
        props_index_cache::lookup_index(&ctx, this, data, 1, "k"),
        Some(0)
    );
    let key_b = crate::gc_stable_weak_lock_key(&ctx, this).unwrap();
    assert_ne!(key_a, key_b, "each VM files its own row");
    assert!(props_index_cache::has_row_for_test(key_b));

    crate::forget_vm_lock_keys(VM_A);
    assert!(!props_index_cache::has_row_for_test(key_a));
    assert!(props_index_cache::has_row_for_test(key_b));
    crate::forget_vm_lock_keys(VM_B);
    assert!(!props_index_cache::has_row_for_test(key_b));
}

/// `Security` providers and properties are one VM's, and go with it.
#[test]
fn security_state_belongs_to_its_vm() {
    const VM_A: usize = 0xC19_0231;
    const VM_B: usize = 0xC19_0232;
    let _teardown = Teardown(&[VM_A, VM_B]);
    assert_eq!(provider_registry_add(VM_A, "X".to_string(), 1.0), 6);
    assert_eq!(provider_registry_find(VM_A, "X"), Some(1.0));
    assert_eq!(provider_registry_find(VM_B, "X"), None);
    assert_eq!(provider_registry_snapshot(VM_B).len(), 5);
    provider_registry_remove(VM_B, "SUN");
    assert_eq!(provider_registry_find(VM_A, "SUN"), Some(21.0));

    set_security_property(VM_A, "keystore.type".to_string(), "JKS".to_string());
    assert_eq!(
        security_property(VM_A, "keystore.type").as_deref(),
        Some("JKS")
    );
    assert_eq!(security_property(VM_B, "keystore.type"), None);

    forget_vm_synthetic_security_state(VM_A);
    assert_eq!(security_property(VM_A, "keystore.type"), None);
    assert_eq!(provider_registry_snapshot(VM_A).len(), 5);
}
