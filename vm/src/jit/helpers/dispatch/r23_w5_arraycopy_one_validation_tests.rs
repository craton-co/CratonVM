use super::*;

fn vm() -> std::sync::Arc<SharedVm> {
    let config = crate::config::VmConfig {
        use_synthetic_jdk: true,
        ..Default::default()
    };
    let vm = std::sync::Arc::new(crate::vm::vm_init::SharedVm::new(config));
    *vm.self_arc.write() = Some(std::sync::Arc::downgrade(&vm));
    vm
}

fn word(obj: ObjectRef) -> i64 {
    obj.as_ptr() as i64
}

fn ints(vm: &SharedVm, n: usize, base: i32) -> ObjectRef {
    let arr = vm.mem.heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
    for i in 0..n {
        let _ = vm.mem.heap.set_array_element(arr, i, Value::Int(base + i as i32));
    }
    arr
}

fn refs(vm: &SharedVm, elems: &[ObjectRef], n: usize) -> ObjectRef {
    let arr = vm.mem.heap.alloc_array(ClassId::new(7), ArrayElementType::Reference, n);
    for (i, e) in elems.iter().enumerate() {
        let _ = vm.mem.heap.set_array_element(arr, i, Value::Object(Some(*e)));
    }
    arr
}

fn dump(vm: &SharedVm, arr: ObjectRef) -> Vec<String> {
    (0..vm.mem.heap.array_length(arr))
        .map(|i| format!("{:?}", vm.mem.heap.get_array_element(arr, i)))
        .collect()
}

/// `(answered, dst after)` for one copy through the new door (`once`) or the
/// old pair of attempts, on fresh arrays built by `make`.
fn run(
    vm: &SharedVm,
    once: bool,
    make: &dyn Fn(&SharedVm) -> (ObjectRef, ObjectRef),
    (sp, dp, len): (i64, i64, i64),
) -> (bool, Vec<String>) {
    let (src, dst) = make(vm);
    // SAFETY: both are live arrays of this heap; nothing allocates below.
    let (s, d) = (word(src), word(dst));
    let answered = unsafe {
        if once {
            try_arraycopy_validated_once(vm, s, sp, d, dp, len, true).is_some()
        } else {
            try_primitive_arraycopy(vm, s, sp, d, dp, len).is_some()
                || try_reference_arraycopy(vm, s, sp, d, dp, len).is_some()
        }
    };
    (answered, dump(vm, dst))
}

#[test]
fn one_validation_answers_what_the_two_attempts_answer() {
    let vm = vm();
    let objs: Vec<ObjectRef> = (0..6)
        .map(|_| vm.mem.heap.alloc_object(ClassId::new(7), 0))
        .collect();
    let shapes: Vec<Box<dyn Fn(&SharedVm) -> (ObjectRef, ObjectRef)>> = vec![
        Box::new(|vm| (ints(vm, 10, 1), ints(vm, 10, 100))),
        Box::new(|vm| {
            let a = ints(vm, 10, 1);
            (a, a)
        }),
        Box::new(|vm| {
            let l = vm.mem.heap.alloc_array(ClassId::new(0), ArrayElementType::Long, 10);
            (ints(vm, 10, 1), l)
        }),
        Box::new({
            let objs = objs.clone();
            move |vm| (refs(vm, &objs, 6), refs(vm, &[], 6))
        }),
        Box::new({
            let objs = objs.clone();
            move |vm| {
                let a = refs(vm, &objs, 6);
                (a, a)
            }
        }),
        Box::new({
            let objs = objs.clone();
            move |vm| (refs(vm, &objs, 6), ints(vm, 6, 0))
        }),
    ];
    let ranges = [
        (0, 0, 3),
        (2, 0, 5),
        (0, 2, 4),
        (1, 0, 9),
        (0, 0, 11),
        (0, 0, 0),
        (-1, 0, 1),
    ];
    for (s, make) in shapes.iter().enumerate() {
        for r in ranges {
            assert_eq!(
                run(&vm, true, make.as_ref(), r),
                run(&vm, false, make.as_ref(), r),
                "shape {s} range {r:?}"
            );
        }
    }
}
