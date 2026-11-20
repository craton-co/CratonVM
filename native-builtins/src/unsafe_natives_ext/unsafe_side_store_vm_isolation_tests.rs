use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{NativeClassAccess, NativeSystemAccess};

// Identities no other test uses (mock contexts default to 0).
const VM_A: usize = 0x7A70_0001;
const VM_B: usize = 0x7A70_0002;

/// The tests below share `VM_A` / `VM_B` and the process-wide per-VM
/// stores, and several end with `forget_vm_unsafe_side_stores`, so run in
/// parallel one test's registrations and forgets land in another's
/// assertions (seen at the wave-8 merge: the synthetic-slot test failed
/// in the module run and passed alone). One test at a time.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn oref(addr: usize) -> ObjectRef {
    // Never dereferenced: the stores only compare and forward addresses,
    // and the mock's identity hash is the address itself.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_of(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn roots_of(vm: usize) -> Vec<ObjectRef> {
    let mut out = Vec::new();
    gc_scan_unsafe_side_store_roots(vm, &mut out);
    out
}

/// The same receiver identity hash and synthetic offset in two VMs -- what
/// two VMs minting hashes from the same counter produce -- are two slots.
#[test]
fn synthetic_slots_with_one_identity_hash_are_per_vm() {
    let _serial = serial();
    let offset = crate::synthetic_offset_for("w7a/SyntheticIsolation", "slot");
    assert!(crate::is_synthetic_offset(offset));
    let recv = oref(0x7A70_1000);
    let (va, vb) = (oref(0x7A70_2000), oref(0x7A70_3000));
    let mut a = ctx_of(VM_A);
    let mut b = ctx_of(VM_B);

    assert!(crate::synthetic_cas(
        &mut a,
        recv,
        offset,
        Value::Object(None),
        Value::Object(Some(va))
    ));
    assert_eq!(
        crate::synthetic_get(&mut b, recv, offset),
        Value::Object(None),
        "VM B must not read VM A's value"
    );
    let (none, some_vb) = (Value::Object(None), Value::Object(Some(vb)));
    let b_won = crate::synthetic_cas(&mut b, recv, offset, none, some_vb);
    assert!(
        b_won,
        "VM B's null -> value CAS must not compare against VM A's value"
    );
    assert_eq!(
        crate::synthetic_get(&mut a, recv, offset),
        Value::Object(Some(va))
    );

    assert!(roots_of(VM_A).contains(&va) && !roots_of(VM_A).contains(&vb));
    assert!(roots_of(VM_B).contains(&vb) && !roots_of(VM_B).contains(&va));

    // B's pointer map never rewrites A's value, even when it names it.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x7A70_2000, 0x7A70_4000);
    gc_update_unsafe_side_store_refs(VM_B, &map);
    assert_eq!(
        crate::synthetic_get(&mut a, recv, offset),
        Value::Object(Some(va))
    );
    gc_update_unsafe_side_store_refs(VM_A, &map);
    assert_eq!(
        crate::synthetic_get(&mut a, recv, offset),
        Value::Object(Some(oref(0x7A70_4000)))
    );

    crate::forget_vm_unsafe_side_stores(VM_A);
    assert_eq!(
        crate::synthetic_get(&mut a, recv, offset),
        Value::Object(None)
    );
    assert_eq!(
        crate::synthetic_get(&mut b, recv, offset),
        Value::Object(Some(vb))
    );
    crate::forget_vm_unsafe_side_stores(VM_B);
    assert!(roots_of(VM_B).is_empty());
}

/// The null-base object store (an Unsafe reference access with a null base
/// at a classified offset nothing registered as a static) is per VM too.
#[test]
fn null_base_object_slots_are_per_vm() {
    let _serial = serial();
    let offset = crate::synthetic_offset_for("w7a/NullBaseIsolation", "static:ghost");
    let va = oref(0x7A70_5000);
    let mut a = ctx_of(VM_A + 2);
    let mut b = ctx_of(VM_B + 2);
    let get = |ctx: &mut MockNativeContext| {
        native_unsafe_get_object(
            ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(offset as i64),
            ],
        )
        .expect("getReference")
        .expect("a value")
    };

    native_unsafe_put_object(
        &mut a,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Long(offset as i64),
            Value::Object(Some(va)),
        ],
    )
    .expect("putReference");
    assert_eq!(get(&mut a), Value::Object(Some(va)));
    assert_eq!(
        get(&mut b),
        Value::Object(None),
        "VM B must not read VM A's slot"
    );
    assert!(roots_of(VM_A + 2).contains(&va));
    assert!(
        !roots_of(VM_B + 2).contains(&va),
        "B's collection must not root A's value"
    );

    crate::forget_vm_unsafe_side_stores(VM_A + 2);
    assert_eq!(get(&mut a), Value::Object(None));
    crate::forget_vm_unsafe_side_stores(VM_B + 2);
}

fn null_base(offset: usize, rest: &[Value]) -> Vec<Value> {
    let mut args = vec![
        Value::Object(None),
        Value::Object(None),
        Value::Long(offset as i64),
    ];
    args.extend_from_slice(rest);
    args
}

/// Interpreter round i5 wave 103, lane L2: two classes of ONE name in ONE
/// VM (an application class and a user loader's copy) get two static
/// offsets, each resolving to its own class's slot. Minted by name, the
/// second registration took the first's offset over, and the application
/// class's reflective static accesses reached the copy.
#[test]
fn same_named_classes_in_one_vm_get_their_own_static_offsets() {
    let _serial = serial();
    let vm = VM_A + 9;
    let (app, copy) = (ClassId::new(595), ClassId::new(1203));
    let name = "i103/SameNamedShape";
    let off_app = crate::register_static_field_offset_in_vm(vm, name, "latched", app, 0);
    let off_copy = crate::register_static_field_offset_in_vm(vm, name, "latched", copy, 0);
    assert_ne!(off_app, off_copy);
    let ctx = ctx_of(vm);
    assert_eq!(
        unsafe_static_field_target(&ctx, off_app as usize),
        Some((app, 0))
    );
    assert_eq!(
        unsafe_static_field_target(&ctx, off_copy as usize),
        Some((copy, 0))
    );
    crate::forget_vm_unsafe_side_stores(vm);
}

/// gc-common w8-a: one `(class, field)` name is one offset in every VM, and
/// two VMs that registered it under DIFFERENT class ids each reach their
/// own static through a null-base access -- never the class the other VM's
/// id happens to name here.
#[test]
fn static_field_offsets_resolve_in_the_calling_vm() {
    let _serial = serial();
    let (vm_a, vm_b) = (VM_A + 4, VM_B + 4);
    let (foo_in_a, foo_in_b) = (ClassId::new(700), ClassId::new(912));
    let name = "w8a/StaticTargetIsolation";
    let off_a = crate::register_static_field_offset_in_vm(vm_a, name, "counter", foo_in_a, 3);
    // Interpreter round i5 wave 103, lane L2: offsets are minted per
    // (name, class id) now, so the two registrations differ; the same id
    // in both VMs would share one. The isolation this test is about is the
    // per-VM table, so VM B registers its class under VM A's offset.
    let off_b = crate::register_static_field_offset_in_vm(vm_b, name, "counter", foo_in_b, 3);
    assert_ne!(off_a, off_b, "two class ids of one name get two offsets");
    assert_eq!(
        crate::register_static_field_offset_in_vm(vm_b, name, "counter", foo_in_a, 3),
        off_a,
        "one (name, id) is one offset in every VM"
    );
    let offset = off_a as usize;
    remember_unsafe_static_field_offset(vm_b, offset, foo_in_b, 3);
    let mut a = ctx_of(vm_a);
    let mut b = ctx_of(vm_b);
    a.set_static_field(foo_in_a, 3, Value::Long(10));
    // What VM B's class id names in VM A: some unrelated class's static.
    a.set_static_field(foo_in_b, 3, Value::Long(99));
    b.set_static_field(foo_in_b, 3, Value::Long(20));
    let add = |ctx: &mut MockNativeContext| {
        native_unsafe_get_and_add_long(ctx, &null_base(offset, &[Value::Long(1)]))
            .expect("getAndAddLong")
            .expect("old value")
    };

    assert_eq!(add(&mut a), Value::Long(10));
    assert_eq!(add(&mut b), Value::Long(20));
    assert_eq!(a.get_static_field(foo_in_a, 3), Value::Long(11));
    assert_eq!(
        a.get_static_field(foo_in_b, 3),
        Value::Long(99),
        "VM B's class id must never be dereferenced in VM A"
    );
    assert_eq!(b.get_static_field(foo_in_b, 3), Value::Long(21));

    // Teardown drops one VM's registration only.
    crate::forget_vm_unsafe_side_stores(vm_a);
    assert!(unsafe_static_field_target(&a, offset).is_none());
    assert_eq!(unsafe_static_field_target(&b, offset), Some((foo_in_b, 3)));
    crate::forget_vm_unsafe_side_stores(vm_b);
}

/// The null-base `int` / `long` fallback stores (a classified offset no VM
/// registered as a static) are per VM, and so is the tid counter behind
/// `Thread$ThreadIdentifiers`.
#[test]
fn null_base_primitive_slots_and_the_tid_counter_are_per_vm() {
    let _serial = serial();
    let (vm_a, vm_b) = (VM_A + 6, VM_B + 6);
    let offset = crate::synthetic_offset_for("w8a/NullBasePrimitive", "native:slot");
    assert!(crate::is_synthetic_offset(offset));
    let mut a = ctx_of(vm_a);
    let mut b = ctx_of(vm_b);

    native_unsafe_put_int(&mut a, &null_base(offset, &[Value::Int(41)])).expect("putInt");
    let get_int = |ctx: &mut MockNativeContext| {
        native_unsafe_get_int(ctx, &null_base(offset, &[]))
            .expect("getInt")
            .expect("a value")
    };
    assert_eq!(get_int(&mut a), Value::Int(41));
    assert_eq!(
        get_int(&mut b),
        Value::Int(0),
        "VM B must not read VM A's slot"
    );
    let cas =
        native_unsafe_cas_int(&mut b, &null_base(offset, &[Value::Int(0), Value::Int(5)]))
            .expect("compareAndSetInt");
    assert_eq!(
        cas,
        Some(Value::Int(1)),
        "B's 0 -> 5 must not compare against A's 41"
    );
    assert_eq!(get_int(&mut a), Value::Int(41));
    assert_eq!(get_int(&mut b), Value::Int(5));

    let long_offset = crate::synthetic_offset_for("w8a/NullBasePrimitive", "native:long");
    let add = |ctx: &mut MockNativeContext| {
        native_unsafe_get_and_add_long(ctx, &null_base(long_offset, &[Value::Long(1)]))
            .expect("getAndAddLong")
            .expect("old value")
    };
    assert_eq!(add(&mut a), Value::Long(0));
    assert_eq!(add(&mut a), Value::Long(1));
    assert_eq!(
        add(&mut b),
        Value::Long(0),
        "B's counter starts at its own seed"
    );

    // Each VM's first application thread gets tid 4 (JDK 25:
    // `Thread.PRIMORDIAL_TID` 3 is `main`'s, the counter starts past it).
    assert_eq!(crate::next_java_thread_tid(vm_a), 4);
    assert_eq!(crate::next_java_thread_tid(vm_a), 5);
    assert_eq!(crate::next_java_thread_tid(vm_b), 4);
    // A VM-built mirror (`mint_vm_thread_java_tid`) draws from the same
    // counter, so it can never collide with a Java-constructed thread.
    assert_eq!(crate::mint_vm_thread_java_tid(vm_a), 6);
    assert_eq!(crate::next_java_thread_tid(vm_a), 7);

    crate::forget_vm_unsafe_side_stores(vm_a);
    assert_eq!(get_int(&mut a), Value::Int(0));
    assert_eq!(
        crate::next_java_thread_tid(vm_a),
        4,
        "teardown drops A's counter"
    );
    assert_eq!(get_int(&mut b), Value::Int(5));
    crate::forget_vm_unsafe_side_stores(vm_a);
    crate::forget_vm_unsafe_side_stores(vm_b);
}

/// The VM-less `register_static_field_offset` (kept until its one caller in
/// `vm_util.rs` passes a VM) records the class NAME, and a VM accepts the
/// registration only for a class of that name in its own VM.
#[test]
fn a_vm_less_static_registration_is_accepted_by_class_name_only() {
    let _serial = serial();
    let (vm_a, vm_b) = (VM_A + 8, VM_B + 8);
    let mut a = ctx_of(vm_a);
    let mut b = ctx_of(vm_b);
    let holder = a
        .ensure_class_initialized("w8a/UnscopedHolder")
        .expect("mock class id");
    a.set_static_field(holder, 1, Value::Int(7));
    // A different class that B may well have minted under the same id.
    let other = b
        .ensure_class_initialized("w8a/NotTheUnscopedHolder")
        .expect("mock class id");
    b.set_static_field(other, 1, Value::Int(55));
    let offset =
        crate::register_static_field_offset("w8a/UnscopedHolder", "latch", holder, 1) as usize;

    assert_eq!(unsafe_static_field_target(&a, offset), Some((holder, 1)));
    let get_int = |ctx: &mut MockNativeContext| {
        native_unsafe_get_int(ctx, &null_base(offset, &[]))
            .expect("getInt")
            .expect("a value")
    };
    assert_eq!(get_int(&mut a), Value::Int(7));
    if other == holder {
        assert_eq!(unsafe_static_field_target(&b, offset), None);
    }
    assert_ne!(
        get_int(&mut b),
        Value::Int(55),
        "B must not reach another class's static"
    );

    crate::forget_vm_unsafe_side_stores(vm_a);
    crate::forget_vm_unsafe_side_stores(vm_b);
}
