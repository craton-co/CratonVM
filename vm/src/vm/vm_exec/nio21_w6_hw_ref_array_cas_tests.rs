use super::*;

fn vm() -> Box<SharedVm> {
    let mut config = crate::config::VmConfig::default();
    config.max_heap_size = 4 * 1024 * 1024;
    config.initial_heap_size = 4 * 1024 * 1024;
    Box::new(SharedVm::new(config))
}

fn raw(v: Value) -> u64 {
    match v {
        Value::Object(Some(r)) => r.as_ptr() as u64,
        _ => 0,
    }
}

/// A reference-element CAS is served in place, answers the lock path's
/// result, leaves the neighbours alone, and reads back through the
/// heap's own element codec (so the slot width matches the heap's).
#[test]
fn nio21_w6_ref_array_element_cas_is_served_in_place() {
    let vm = vm();
    let arr = vm.mem.heap.alloc_array(
        ClassId::new(0),
        cratonvm_types::ArrayElementType::Reference,
        3,
    );
    let a = Value::Object(Some(vm.mem.heap.alloc_array(
        ClassId::new(0),
        cratonvm_types::ArrayElementType::Int,
        1,
    )));
    let b = Value::Object(Some(vm.mem.heap.alloc_array(
        ClassId::new(0),
        cratonvm_types::ArrayElementType::Int,
        1,
    )));
    let null = Value::Object(None);
    assert!(
        compare_and_swap_field_shared(&vm, arr, 1, null, a),
        "null -> a"
    );
    assert_eq!(raw(vm.mem.heap.get_array_element(arr, 1).unwrap()), raw(a));
    assert!(
        !compare_and_swap_field_shared(&vm, arr, 1, null, b),
        "stale expected"
    );
    let (w, ok) = hw_cas_reference_array_element(&vm, arr, 1, a, b).expect("served");
    assert!(ok && w == raw(a), "witness on success");
    let (w, ok) = hw_cas_reference_array_element(&vm, arr, 1, a, null).expect("served");
    assert!(!ok && w == raw(b), "witness on failure");
    assert_eq!(raw(vm.mem.heap.get_array_element(arr, 1).unwrap()), raw(b));
    assert_eq!(raw(vm.mem.heap.get_array_element(arr, 0).unwrap()), 0);
    assert_eq!(raw(vm.mem.heap.get_array_element(arr, 2).unwrap()), 0);
    assert!(
        hw_cas_reference_array_element(&vm, arr, 3, null, a).is_none(),
        "out of range"
    );
    // Not a reference array, or not a reference pair: declined.
    let ia = vm
        .mem
        .heap
        .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Int, 1);
    assert!(hw_cas_reference_array_element(&vm, ia, 0, null, a).is_none());
    assert!(hw_cas_reference_array_element(&vm, arr, 0, Value::Int(0), a).is_none());
}

/// Four threads toggling one element between two markers with CAS: every
/// reported success really happened, so the final marker is fixed by the
/// parity of the success count. A lost or phantom swap breaks it.
#[test]
fn nio21_w6_ref_array_element_cas_counts_are_exact_across_threads() {
    let vm = vm();
    let arr = vm.mem.heap.alloc_array(
        ClassId::new(0),
        cratonvm_types::ArrayElementType::Reference,
        1,
    );
    let m = [
        vm.mem
            .heap
            .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Int, 1),
        vm.mem
            .heap
            .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Int, 1),
    ];
    let _ = vm
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(m[0])));
    let addrs = [
        arr.as_ptr() as usize,
        m[0].as_ptr() as usize,
        m[1].as_ptr() as usize,
    ];
    let vm_ref: &SharedVm = &vm;
    let swaps = std::sync::atomic::AtomicUsize::new(0);
    let swaps_ref = &swaps;
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(move || {
                // SAFETY: the objects are live for the whole scope, and
                // nothing allocates or collects in this test.
                let r = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
                let (arr, x, y) = (r(addrs[0]), r(addrs[1]), r(addrs[2]));
                for _ in 0..20_000 {
                    for (e, n) in [(x, y), (y, x)] {
                        if compare_and_swap_field_shared(
                            vm_ref,
                            arr,
                            0,
                            Value::Object(Some(e)),
                            Value::Object(Some(n)),
                        ) {
                            swaps_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    });
    // Every successful swap toggles the slot between the two markers, so
    // the final value is decided by the swap count's parity.
    let n = swaps.load(std::sync::atomic::Ordering::Relaxed);
    let want = if n % 2 == 0 { m[0] } else { m[1] };
    assert_eq!(
        raw(vm.mem.heap.get_array_element(arr, 0).unwrap()),
        want.as_ptr() as u64
    );
}
