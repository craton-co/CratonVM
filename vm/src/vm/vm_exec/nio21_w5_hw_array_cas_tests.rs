use super::*;
use cratonvm_types::ArrayElementType as E;

fn vm_with_array(et: E, len: usize) -> (Box<SharedVm>, ObjectRef) {
    let mut config = crate::config::VmConfig::default();
    config.max_heap_size = 4 * 1024 * 1024;
    config.initial_heap_size = 4 * 1024 * 1024;
    let vm: Box<SharedVm> = Box::new(SharedVm::new(config));
    let arr = vm.mem.heap.alloc_array(ClassId::new(0), et, len);
    (vm, arr)
}

/// The expected/new encodings are the heap's own: what `write_prim_element`
/// stores for `new`, and an `expected` the element can hold exactly when
/// `read_prim_element` can produce it.
#[test]
fn nio21_w5_hw_array_cas_bits_follow_the_element_codec() {
    let b = hw_array_cas_bits(E::Byte, Value::Int(-1), Value::Int(0x17f)).unwrap();
    assert_eq!(
        (b.width, b.expected, b.new, b.expected_representable),
        (1, 0xff, 0x7f, true)
    );
    let b = hw_array_cas_bits(E::Byte, Value::Int(200), Value::Int(1)).unwrap();
    assert!(
        !b.expected_representable,
        "a byte reads sign-extended: 200 is never stored"
    );
    let z = hw_array_cas_bits(E::Boolean, Value::Int(1), Value::Int(2)).unwrap();
    assert_eq!(
        (z.width, z.expected, z.new),
        (1, 1, 0),
        "bastore narrows to v & 1"
    );
    let c = hw_array_cas_bits(E::Char, Value::Int(0xffff), Value::Int(-1)).unwrap();
    assert_eq!(
        (c.width, c.expected, c.new, c.expected_representable),
        (2, 0xffff, 0xffff, true)
    );
    assert!(
        !hw_array_cas_bits(E::Char, Value::Int(-1), Value::Int(0))
            .unwrap()
            .expected_representable
    );
    let s = hw_array_cas_bits(E::Short, Value::Int(-2), Value::Int(0x12345)).unwrap();
    assert_eq!(
        (s.width, s.expected, s.new, s.expected_representable),
        (2, 0xfffe, 0x2345, true)
    );
    let i = hw_array_cas_bits(E::Int, Value::Int(-1), Value::Int(7)).unwrap();
    assert_eq!((i.width, i.expected, i.new), (4, 0xffff_ffff, 7));
    let l = hw_array_cas_bits(E::Long, Value::Long(-1), Value::Long(7)).unwrap();
    assert_eq!((l.width, l.expected, l.new), (8, u64::MAX, 7));
    let f = hw_array_cas_bits(E::Float, Value::Float(-0.0), Value::Float(1.0)).unwrap();
    assert_eq!((f.width, f.expected), (4, u64::from((-0.0f32).to_bits())));
    // Pairs the lock path keeps: mismatched tags, references, the
    // `Object(None)`-as-zero coercion.
    assert!(hw_array_cas_bits(E::Int, Value::Long(0), Value::Long(1)).is_none());
    assert!(hw_array_cas_bits(E::Long, Value::Int(0), Value::Int(1)).is_none());
    assert!(hw_array_cas_bits(E::Int, Value::Object(None), Value::Int(1)).is_none());
    assert!(
        hw_array_cas_bits(E::Reference, Value::Object(None), Value::Object(None)).is_none()
    );
    for (et, bits) in [
        (E::Byte, 0x80u64),
        (E::Boolean, 2),
        (E::Char, 0x8000),
        (E::Short, 0x8000),
    ] {
        let v = hw_array_decode(et, bits);
        let enc = hw_array_cas_bits(et, v, v).unwrap();
        assert!(enc.expected_representable && enc.expected == bits, "{et:?}");
    }
}

/// An element CAS answers what the lock path answered, and writes the
/// element in place, for every primitive element type.
#[test]
fn nio21_w5_array_element_cas_is_served_in_place() {
    let cases: [(E, Value, Value, Value); 8] = [
        (E::Int, Value::Int(0), Value::Int(-5), Value::Int(9)),
        (
            E::Long,
            Value::Long(0),
            Value::Long(i64::MIN),
            Value::Long(3),
        ),
        (E::Byte, Value::Int(0), Value::Int(-1), Value::Int(4)),
        (E::Boolean, Value::Int(0), Value::Int(1), Value::Int(0)),
        (E::Char, Value::Int(0), Value::Int(0xfffe), Value::Int(4)),
        (E::Short, Value::Int(0), Value::Int(-300), Value::Int(4)),
        (
            E::Float,
            Value::Float(0.0),
            Value::Float(1.5),
            Value::Float(2.5),
        ),
        (
            E::Double,
            Value::Double(0.0),
            Value::Double(-2.5),
            Value::Double(8.0),
        ),
    ];
    for (et, zero, a, b) in cases {
        let (vm, arr) = vm_with_array(et, 3);
        assert!(
            compare_and_swap_field_shared(&vm, arr, 1, zero, a),
            "{et:?} 0 -> a"
        );
        assert!(values_equal_for_cas(
            &vm.mem.heap.get_array_element(arr, 1).unwrap(),
            &a
        ));
        assert!(
            !compare_and_swap_field_shared(&vm, arr, 1, zero, b),
            "{et:?} stale expected"
        );
        let (w, ok) = hw_cas_primitive_array_element(&vm, arr, 1, a, b).expect("served");
        assert!(ok && values_equal_for_cas(&w, &a), "{et:?} witness");
        let (w, ok) = hw_cas_primitive_array_element(&vm, arr, 1, a, b).expect("served");
        assert!(!ok && values_equal_for_cas(&w, &b), "{et:?} failed witness");
        // Neighbours untouched.
        assert!(values_equal_for_cas(
            &vm.mem.heap.get_array_element(arr, 0).unwrap(),
            &zero
        ));
        assert!(values_equal_for_cas(
            &vm.mem.heap.get_array_element(arr, 2).unwrap(),
            &zero
        ));
        // Out of range declines (the natives bounds-check before this).
        assert!(hw_cas_primitive_array_element(&vm, arr, 3, zero, a).is_none());
    }
}

/// Four threads of CAS increments on one `int[]` element and one `long[]`
/// element lose nothing.
#[test]
fn nio21_w5_array_element_cas_counts_are_exact_across_threads() {
    let (vm, ia) = vm_with_array(E::Int, 2);
    let la = vm.mem.heap.alloc_array(ClassId::new(0), E::Long, 2);
    let (ia_addr, la_addr) = (ia.as_ptr() as usize, la.as_ptr() as usize);
    let vm_ref: &SharedVm = &vm;
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(move || {
                // SAFETY: both arrays are live for the whole scope, and
                // nothing allocates or collects in this test.
                let vm = vm_ref;
                let ia = unsafe { ObjectRef::from_raw(ia_addr as *mut u8) };
                let la = unsafe { ObjectRef::from_raw(la_addr as *mut u8) };
                for _ in 0..20_000 {
                    loop {
                        let cur = vm.mem.heap.get_array_element(ia, 1).unwrap();
                        let Value::Int(v) = cur else { panic!() };
                        if compare_and_swap_field_shared(vm, ia, 1, cur, Value::Int(v + 1)) {
                            break;
                        }
                    }
                    loop {
                        let cur = vm.mem.heap.get_array_element(la, 0).unwrap();
                        let Value::Long(v) = cur else { panic!() };
                        if compare_and_swap_field_shared(vm, la, 0, cur, Value::Long(v + 1)) {
                            break;
                        }
                    }
                }
            });
        }
    });
    assert_eq!(
        vm.mem.heap.get_array_element(ia, 1).unwrap(),
        Value::Int(80_000)
    );
    assert_eq!(
        vm.mem.heap.get_array_element(la, 0).unwrap(),
        Value::Long(80_000)
    );
}
