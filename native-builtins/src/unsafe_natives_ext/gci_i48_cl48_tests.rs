use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Identities no other test uses (mock contexts default to 0).
const VM: usize = 0xC148_0001;
const NAME: &str = "gci48cl48/Holder";

/// `n` same-named classes (the first by name, the rest as second copies),
/// each with its static `graph` registered as `staticFieldOffset`
/// registers it, in order. Since dev's `eab6d094dd` a static offset
/// carries the class id, so every copy gets its own offset: returns the
/// classes and their offsets, index for index.
fn copies(ctx: &mut MockNativeContext, vm: usize, n: usize) -> (Vec<ClassId>, Vec<usize>) {
    let mut classes = vec![ctx.ensure_class_initialized(NAME).expect("mock class id")];
    for _ in 1..n {
        classes.push(ctx.declare_second_copy(NAME));
    }
    let offsets: Vec<usize> = classes
        .iter()
        .map(|&c| crate::register_static_field_offset_in_vm(vm, NAME, "graph", c, 0) as usize)
        .collect();
    let mut distinct = offsets.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), offsets.len(), "one copy, one offset");
    (classes, offsets)
}

#[test]
fn atm21_static_publication_refusal_follows_the_registration() {
    let vm = 0xA721_0001;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    let class = ctx.ensure_class_initialized("atm21/Refusal").unwrap();
    ctx.set_declared_fields(
        class,
        vec![cratonvm_native_api::FieldMetadata {
            name: "value".into(),
            descriptor: "Ljava/lang/Object;".into(),
            access_flags: 0x0008,
            slot_index: 0,
            declaring_class_id: class,
            is_static: true,
        }],
    );
    ctx.set_static_field(class, 0, Value::Object(None));
    let mirror = ctx.get_class_mirror(class);
    let offset =
        crate::register_static_field_offset_in_vm(vm, "atm21/Refusal", "value", class, 0)
            as usize;
    assert!(synthetic_cas(
        &mut ctx,
        mirror,
        offset,
        Value::Object(None),
        Value::Object(None),
    ));
    assert_eq!(ctx.unsafe_static_publish_attempts.get(), 1);
    assert!(synthetic_cas(
        &mut ctx,
        mirror,
        offset,
        Value::Object(None),
        Value::Object(None),
    ));
    assert_eq!(ctx.unsafe_static_publish_attempts.get(), 1);

    // A replacement registration must resolve its descriptor again.
    remember_unsafe_static_field_offset(vm, offset, class, 0);
    assert!(synthetic_cas(
        &mut ctx,
        mirror,
        offset,
        Value::Object(None),
        Value::Object(None),
    ));
    assert_eq!(ctx.unsafe_static_publish_attempts.get(), 2);
    crate::forget_vm_unsafe_side_stores(vm);
}

#[test]
fn gci_i48_cl48_each_copy_reads_back_its_own_static_through_its_mirror() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let (classes, offsets) = copies(&mut ctx, VM, 8);
    let mirrors: Vec<ObjectRef> = classes.iter().map(|&c| ctx.get_class_mirror(c)).collect();
    let values: Vec<ObjectRef> = classes
        .iter()
        .map(|_| ctx.alloc_object(ClassId::new(0), 1))
        .collect();
    let based = |base: ObjectRef, offset: usize, rest: &[Value]| {
        let mut a = vec![
            Value::Object(None),
            Value::Object(Some(base)),
            Value::Long(offset as i64),
        ];
        a.extend_from_slice(rest);
        a
    };
    for (k, &m) in mirrors.iter().enumerate() {
        native_unsafe_put_object(&mut ctx, &based(m, offsets[k], &[Value::Object(Some(values[k]))]))
            .expect("putReference");
    }
    for (k, &m) in mirrors.iter().enumerate() {
        let got = native_unsafe_get_object(&mut ctx, &based(m, offsets[k], &[]))
            .expect("getReference")
            .expect("a value");
        assert_eq!(
            got,
            Value::Object(Some(values[k])),
            "copy {k} reads its own value"
        );
        // The REAL static, which `getstatic` reads -- not a side slot.
        assert_eq!(
            ctx.get_static_field(classes[k], 0),
            Value::Object(Some(values[k])),
            "copy {k}'s static holds its own value"
        );
    }
    crate::forget_vm_unsafe_side_stores(VM);
}

#[test]
fn gci_i48_cl48_a_base_names_its_copy_and_no_base_takes_the_latest() {
    let vm = VM + 1;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    let (classes, offsets) = copies(&mut ctx, vm, 3);
    for (k, &c) in classes.iter().enumerate() {
        let off = offsets[k];
        assert_eq!(unsafe_static_field_target(&ctx, off), Some((c, 0)));
        assert_eq!(unsafe_static_field_target_via_base(&ctx, None, off), Some((c, 0)));
        let m = ctx.get_class_mirror(c);
        assert_eq!(unsafe_static_field_target_for_base(&ctx, m, off), Some((c, 0)));
        assert_eq!(unsafe_static_field_target_via_base(&ctx, Some(m), off), Some((c, 0)));
        // Another copy's mirror never names this copy's offset.
        for &other in classes.iter().filter(|&&o| o != c) {
            let om = ctx.get_class_mirror(other);
            assert_eq!(unsafe_static_field_target_for_base(&ctx, om, off), None);
        }
    }
    // A copy registered again keeps its offset; the row keeps one entry.
    let again = crate::register_static_field_offset_in_vm(vm, NAME, "graph", classes[0], 0);
    assert_eq!(again as usize, offsets[0]);
    let rows = lock_unsafe_vm_offset_shard(unsafe_static_field_targets(), offsets[0])
        .get(&(vm, offsets[0]))
        .map(Vec::len);
    assert_eq!(rows, Some(1));
    crate::forget_vm_unsafe_side_stores(vm);
    assert_eq!(unsafe_static_field_target(&ctx, offsets[0]), None);
}

/// gcj j1/jt (page `gci-i48-unsafe-byte-lane-static-sub-target-ignores-the-base`):
/// byte `k != 0` of a static (byte field offsets) resolves its containing
/// static against the access's base mirror, as the word lane does, so two
/// copies sharing one scaled offset each read, write and CAS their own
/// static; no base keeps the latest registration.
#[test]
fn gcj1jt_the_byte_lane_of_a_static_resolves_against_its_base() {
    let vm = VM + 2;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx.unsafe_byte_field_offsets_flag.set(true);
    let (classes, offsets) = copies(&mut ctx, vm, 2);
    let (c0, c1) = (classes[0], classes[1]);
    let scaled = unsafe_static_offset_answer(&ctx, offsets[0], c0, 0);
    assert!(scaled >= UNSAFE_BYTE_STATIC_OFFSET_BASE && scaled & 7 == 0);
    // The second copy registered under the SAME scaled offset: a shared
    // row, the shape a name-keyed mint produces.
    remember_unsafe_static_field_offset(vm, scaled, c1, 0);
    ctx.set_static_field(c0, 0, Value::Int(0x1122_3344));
    ctx.set_static_field(c1, 0, Value::Int(0x5566_7788));
    let m0 = ctx.get_class_mirror(c0);
    let m1 = ctx.get_class_mirror(c1);
    let t0 = unsafe_static_sub_target(&ctx, Some(m0), scaled + 1);
    assert_eq!(t0, Some((c0, 0, 1)));
    let t1 = unsafe_static_sub_target(&ctx, Some(m1), scaled + 1);
    assert_eq!(t1, Some((c1, 0, 1)));
    // No base: the latest registration, as before.
    let latest = unsafe_static_sub_target(&ctx, None, scaled + 1);
    assert_eq!(latest, Some((c1, 0, 1)));
    // The exact offset is never a sub-target.
    assert_eq!(unsafe_static_sub_target(&ctx, Some(m0), scaled), None);
    let g0 = unsafe_static_sub_get(&ctx, Some(m0), scaled + 1);
    assert_eq!(g0, Some(Value::Int(0x0011_2233)));
    let g1 = unsafe_static_sub_get(&ctx, Some(m1), scaled + 1);
    assert_eq!(g1, Some(Value::Int(0x0055_6677)));
    // An int store at byte 2 of copy 0 (clamped to the field's two upper
    // bytes) leaves copy 1 untouched.
    let wrote = unsafe_static_sub_put(&mut ctx, Some(m0), scaled + 2, Value::Int(0x7f));
    assert!(wrote);
    assert_eq!(ctx.get_static_field(c0, 0), Value::Int(0x007f_3344));
    assert_eq!(ctx.get_static_field(c1, 0), Value::Int(0x5566_7788));
    // A CAS on copy 1's bytes 1..3 sees copy 1's witness, not copy 0's.
    let (ex, nv) = (Value::Int(0x0055_6677), Value::Int(1));
    let swapped = unsafe_static_sub_cas(&mut ctx, Some(m1), scaled + 1, ex, nv);
    assert_eq!(swapped, Some(true));
    assert_eq!(ctx.get_static_field(c1, 0), Value::Int(0x0000_0188));
    assert_eq!(ctx.get_static_field(c0, 0), Value::Int(0x007f_3344));
    crate::forget_vm_unsafe_side_stores(vm);
}
