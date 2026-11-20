use super::*;

const PRIMITIVES: [ArrayElementType; 8] = [
    ArrayElementType::Boolean,
    ArrayElementType::Byte,
    ArrayElementType::Char,
    ArrayElementType::Short,
    ArrayElementType::Int,
    ArrayElementType::Float,
    ArrayElementType::Long,
    ArrayElementType::Double,
];

fn vm() -> std::sync::Arc<SharedVm> {
    let config = crate::config::VmConfig {
        use_synthetic_jdk: true,
        ..Default::default()
    };
    let vm = std::sync::Arc::new(crate::vm::vm_init::SharedVm::new(config));
    *vm.self_arc.write() = Some(std::sync::Arc::downgrade(&vm));
    vm
}

fn payload(vm: &SharedVm, arr: ObjectRef, et: ArrayElementType) -> Vec<u8> {
    let bytes = vm.mem.heap.array_length(arr) * cratonvm_types::element_byte_size(et);
    let base = vm.mem.heap.array_data_ptr(arr).expect("a live array has a payload");
    // SAFETY: the payload of the live array `arr`, `bytes` long.
    unsafe { std::slice::from_raw_parts(base as *const u8, bytes).to_vec() }
}

/// A fresh `n`-element array of `et` whose payload bytes are a pattern.
fn patterned(vm: &SharedVm, et: ArrayElementType, n: usize) -> ObjectRef {
    let arr = vm.mem.heap.alloc_array(ClassId::new(0), et, n);
    let bytes = n * cratonvm_types::element_byte_size(et);
    let base = vm.mem.heap.array_data_ptr(arr).expect("a live array has a payload");
    for i in 0..bytes {
        // SAFETY: `i` is inside the payload of the fresh array.
        unsafe { *base.add(i) = (i as u8).wrapping_mul(31).wrapping_add(7) };
    }
    arr
}

fn elements(vm: &SharedVm, arr: ObjectRef) -> Vec<String> {
    (0..vm.mem.heap.array_length(arr))
        .map(|i| format!("{:?}", vm.mem.heap.get_array_element(arr, i)))
        .collect()
}

/// Every primitive kind, zero length included: the copy holds the whole
/// payload and the original is unchanged.
#[test]
fn j27_every_primitive_array_is_copied_whole_and_the_original_kept() {
    let vm = vm();
    for et in PRIMITIVES {
        for n in [0usize, 1, 7, 64] {
            let src = patterned(&vm, et, n);
            let before = payload(&vm, src, et);
            let copy = vm.mem.heap.alloc_array(ClassId::new(0), et, n);
            // SAFETY: two live arrays of this heap; nothing allocates below.
            let done = unsafe { array_clone_copy(&vm, src, copy, et, n) };
            assert_eq!(done, Some(()), "{et:?} x{n}");
            assert_eq!(payload(&vm, copy, et), before, "{et:?} x{n}: the copy");
            assert_eq!(payload(&vm, src, et), before, "{et:?} x{n}: the original");
        }
    }
}

/// A reference array, a null slot included: the copy names the same
/// objects in the same slots, and the original still does.
#[test]
fn j27_a_reference_array_is_copied_slot_for_slot() {
    let vm = vm();
    let component = ClassId::new(7);
    let objs: Vec<ObjectRef> = (0..5).map(|_| vm.mem.heap.alloc_object(component, 0)).collect();
    let src = vm.mem.heap.alloc_array(component, ArrayElementType::Reference, 6);
    for (i, o) in objs.iter().enumerate() {
        let _ = vm.mem.heap.set_array_element(src, i, Value::Object(Some(*o)));
    }
    let before = elements(&vm, src);
    let copy = vm.mem.heap.alloc_array(component, ArrayElementType::Reference, 6);
    // SAFETY: two live reference arrays of this heap; nothing allocates below.
    let done = unsafe { array_clone_copy(&vm, src, copy, ArrayElementType::Reference, 6) };
    assert_eq!(done, Some(()));
    assert_eq!(elements(&vm, copy), before);
    assert_eq!(elements(&vm, src), before);
    assert_eq!(before[5], format!("{:?}", vm.mem.heap.get_array_element(copy, 5)));
}

/// The declines are the site's own VIRTUAL call (an override must be
/// selected), and the compiler's question answers nothing it cannot ask.
#[test]
fn j27_the_decline_is_virtual_and_the_question_refuses_no_vm_or_object() {
    assert_eq!(CLONE_VIRTUAL_INFO.invoke_kind, 0);
    assert_eq!(
        (CLONE_VIRTUAL_INFO.class_name, CLONE_VIRTUAL_INFO.method_name),
        ("java/lang/Object", "clone")
    );
    let vm = vm();
    let ctx = &*vm as *const SharedVm as i64;
    // SAFETY: `ctx` is the live VM above, or 0.
    unsafe {
        assert_eq!(jit_object_clone_served_class(0, 9), 0);
        assert_eq!(jit_object_clone_served_class(ctx, 0), 0);
        assert_eq!(jit_object_clone_served_class(ctx, -1), 0);
    }
}

/// j27 wave 4 (lane CLONE2): the compiler's question never waits for the
/// class manager. With the write side held (a class load in flight) it
/// answers "not served" at once; it used to block in `read()` for as long
/// as the writer held it (here, this very thread: forever).
#[test]
fn j27_a_busy_class_manager_answers_not_served_without_waiting() {
    let vm = vm();
    let ctx = &*vm as *const SharedVm as i64;
    let held = vm.classes.class_manager.write();
    // SAFETY: `ctx` is the live VM above.
    let answer = unsafe { jit_object_clone_served_class(ctx, 0x4d1) };
    drop(held);
    assert_eq!(answer, 0);
}

/// A busy answer is `0` and is not remembered; a computed one is, and a
/// later ask does not compute again.
#[test]
fn j27_a_busy_verdict_is_not_memoised_and_a_computed_one_is() {
    const BOTH: u8 = OBJECT_CLONE_PLAIN_BIT | OBJECT_CLONE_INHERITED_BIT;
    let vm = vm();
    let class = ClassId::new(0x4d2);
    assert_eq!(object_clone_class_verdict_by(&vm, class, || None), 0);
    assert_eq!(object_clone_class_verdict_by(&vm, class, || Some(BOTH)), BOTH);
    let again = object_clone_class_verdict_by(&vm, class, || panic!("asked twice"));
    assert_eq!(again, BOTH);
}

/// j27 wave 4 (lane CLONE2): a class the compact copy does not cover is
/// served by neither clone helper, however plain and inherited it is, so
/// both decline before the bump; with the bit it is served by both.
#[test]
fn j27_a_class_the_compact_copy_refuses_is_not_served() {
    const PLAIN_INHERITED: u8 = OBJECT_CLONE_PLAIN_BIT | OBJECT_CLONE_INHERITED_BIT;
    let vm = vm();
    let (refused, covered) = (ClassId::new(0x4d3), ClassId::new(0x4d4));
    // Seed the memo with each class's verdict (see the test above).
    object_clone_class_verdict_by(&vm, refused, || Some(PLAIN_INHERITED));
    object_clone_class_verdict_by(&vm, covered, || {
        Some(PLAIN_INHERITED | OBJECT_CLONE_COMPACT_COPY_BIT)
    });
    assert!(!object_clone_is_plain(&vm, refused));
    assert!(!object_clone_virtual_serves(&vm, refused));
    assert!(object_clone_is_plain(&vm, covered));
    assert!(object_clone_virtual_serves(&vm, covered));
}
