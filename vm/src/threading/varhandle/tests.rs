use super::*;

// -- AccessMode --

#[test]
fn access_mode_all_31_variants() {
    assert_eq!(AccessMode::all().len(), ACCESS_MODE_COUNT);
    assert_eq!(AccessMode::all().len(), 31);
}

#[test]
fn access_mode_variants_are_unique() {
    let all = AccessMode::all();
    for (i, a) in all.iter().enumerate() {
        for (j, b) in all.iter().enumerate() {
            if i != j {
                assert_ne!(a, b, "duplicate variant at indices {i} and {j}");
            }
        }
    }
}

// -- VarType --

#[test]
fn var_type_all_9_variants() {
    assert_eq!(VarType::all().len(), 9);
}

#[test]
fn var_type_numeric_classification() {
    assert!(VarType::Int.is_numeric());
    assert!(VarType::Long.is_numeric());
    assert!(VarType::Float.is_numeric());
    assert!(VarType::Double.is_numeric());
    assert!(!VarType::Object.is_numeric());
    assert!(!VarType::Boolean.is_numeric());
}

#[test]
fn var_type_integer_classification() {
    assert!(VarType::Int.is_integer());
    assert!(VarType::Long.is_integer());
    assert!(VarType::Byte.is_integer());
    assert!(VarType::Short.is_integer());
    assert!(VarType::Char.is_integer());
    assert!(!VarType::Float.is_integer());
    assert!(!VarType::Object.is_integer());
}

// -- VarHandleKind --

#[test]
fn varhandle_kind_instance_field() {
    let kind = VarHandleKind::InstanceField {
        class_name: "java/lang/Object".into(),
        field_name: "value".into(),
        field_index: 0,
    };
    assert_eq!(
        kind,
        VarHandleKind::InstanceField {
            class_name: "java/lang/Object".into(),
            field_name: "value".into(),
            field_index: 0,
        }
    );
}

#[test]
fn varhandle_kind_static_field() {
    let kind = VarHandleKind::StaticField {
        class_name: "java/lang/System".into(),
        field_name: "out".into(),
        field_index: 2,
    };
    if let VarHandleKind::StaticField {
        class_name,
        field_name,
        field_index,
    } = &kind
    {
        assert_eq!(class_name, "java/lang/System");
        assert_eq!(field_name, "out");
        assert_eq!(*field_index, 2);
    } else {
        panic!("expected StaticField");
    }
}

#[test]
fn varhandle_kind_array_element() {
    let kind = VarHandleKind::ArrayElement {
        element_type: VarType::Int,
    };
    assert_eq!(
        kind,
        VarHandleKind::ArrayElement {
            element_type: VarType::Int
        }
    );
}

#[test]
fn varhandle_kind_byte_buffer_view() {
    let kind = VarHandleKind::ByteBufferView {
        element_type: VarType::Long,
    };
    assert_eq!(
        kind,
        VarHandleKind::ByteBufferView {
            element_type: VarType::Long
        }
    );
}

// -- MemoryOrdering --

#[test]
fn memory_ordering_plain_is_relaxed() {
    assert_eq!(MemoryOrdering::plain(), Ordering::Relaxed);
}

#[test]
fn memory_ordering_opaque_is_relaxed() {
    assert_eq!(MemoryOrdering::opaque(), Ordering::Relaxed);
}

#[test]
fn memory_ordering_acquire() {
    assert_eq!(MemoryOrdering::acquire(), Ordering::Acquire);
}

#[test]
fn memory_ordering_release() {
    assert_eq!(MemoryOrdering::release(), Ordering::Release);
}

#[test]
fn memory_ordering_volatile_is_seqcst() {
    assert_eq!(MemoryOrdering::volatile(), Ordering::SeqCst);
}

#[test]
fn memory_ordering_for_access_mode_plain() {
    assert_eq!(
        MemoryOrdering::for_access_mode(AccessMode::Get),
        Ordering::Relaxed
    );
    assert_eq!(
        MemoryOrdering::for_access_mode(AccessMode::Set),
        Ordering::Relaxed
    );
}

#[test]
fn memory_ordering_for_access_mode_acquire() {
    assert_eq!(
        MemoryOrdering::for_access_mode(AccessMode::GetAcquire),
        Ordering::Acquire
    );
    assert_eq!(
        MemoryOrdering::for_access_mode(AccessMode::CompareAndExchangeAcquire),
        Ordering::Acquire
    );
}

#[test]
fn memory_ordering_for_access_mode_release() {
    assert_eq!(
        MemoryOrdering::for_access_mode(AccessMode::SetRelease),
        Ordering::Release
    );
    assert_eq!(
        MemoryOrdering::for_access_mode(AccessMode::GetAndSetRelease),
        Ordering::Release
    );
}

#[test]
fn memory_ordering_for_access_mode_volatile() {
    assert_eq!(
        MemoryOrdering::for_access_mode(AccessMode::GetVolatile),
        Ordering::SeqCst
    );
    assert_eq!(
        MemoryOrdering::for_access_mode(AccessMode::CompareAndSet),
        Ordering::SeqCst
    );
    assert_eq!(
        MemoryOrdering::for_access_mode(AccessMode::GetAndAdd),
        Ordering::SeqCst
    );
}

// -- FenceOperations --

#[test]
fn fence_full_does_not_panic() {
    FenceOperations::full_fence();
}

#[test]
fn fence_acquire_does_not_panic() {
    FenceOperations::acquire_fence();
}

#[test]
fn fence_release_does_not_panic() {
    FenceOperations::release_fence();
}

#[test]
fn fence_load_load_does_not_panic() {
    FenceOperations::load_load_fence();
}

#[test]
fn fence_store_store_does_not_panic() {
    FenceOperations::store_store_fence();
}

// -- AtomicOperations: int --

#[test]
fn atomic_cas_int_success() {
    let (ok, witness) = AtomicOperations::compare_and_set_int(42, 42, 99, Ordering::SeqCst);
    assert!(ok);
    assert_eq!(witness, 42);
}

#[test]
fn atomic_cas_int_failure() {
    let (ok, witness) = AtomicOperations::compare_and_set_int(42, 10, 99, Ordering::SeqCst);
    assert!(!ok);
    assert_eq!(witness, 42);
}

#[test]
fn atomic_compare_and_exchange_int() {
    let prev = AtomicOperations::compare_and_exchange_int(
        42,
        42,
        99,
        Ordering::SeqCst,
        Ordering::Relaxed,
    );
    assert_eq!(prev, 42);
}

#[test]
fn atomic_compare_and_exchange_int_fail() {
    let prev = AtomicOperations::compare_and_exchange_int(
        42,
        10,
        99,
        Ordering::SeqCst,
        Ordering::Relaxed,
    );
    assert_eq!(prev, 42); // witness is old value, not new
}

#[test]
fn atomic_get_and_set_int() {
    let old = AtomicOperations::get_and_set_int(42, 99);
    assert_eq!(old, 42);
}

#[test]
fn atomic_get_and_add_int() {
    let old = AtomicOperations::get_and_add_int(42, 8);
    assert_eq!(old, 42);
}

#[test]
fn atomic_get_and_or_int() {
    let old = AtomicOperations::get_and_or_int(0b1010, 0b1100);
    assert_eq!(old, 0b1010);
}

#[test]
fn atomic_get_and_and_int() {
    let old = AtomicOperations::get_and_and_int(0b1010, 0b1100);
    assert_eq!(old, 0b1010);
}

#[test]
fn atomic_get_and_xor_int() {
    let old = AtomicOperations::get_and_xor_int(0b1010, 0b1100);
    assert_eq!(old, 0b1010);
}

// -- AtomicOperations: long --

#[test]
fn atomic_cas_long_success() {
    let (ok, witness) =
        AtomicOperations::compare_and_set_long(100_000, 100_000, 200_000, Ordering::SeqCst);
    assert!(ok);
    assert_eq!(witness, 100_000);
}

#[test]
fn atomic_get_and_add_long() {
    let old = AtomicOperations::get_and_add_long(1_000_000, 500);
    assert_eq!(old, 1_000_000);
}

#[test]
fn atomic_get_and_set_long() {
    let old = AtomicOperations::get_and_set_long(42, 99);
    assert_eq!(old, 42);
}

#[test]
fn atomic_get_and_or_long() {
    let old = AtomicOperations::get_and_or_long(0xFF00, 0x00FF);
    assert_eq!(old, 0xFF00);
}

// -- VarHandleRegistry --

#[test]
fn registry_create_instance_field_handle() {
    let mut reg = VarHandleRegistry::new();
    let id = reg.create_instance_field_handle("Foo", "bar", 0, VarType::Int);
    assert!(id > 0);
    let h = reg.get(id).unwrap();
    assert_eq!(h.var_type, VarType::Int);
    if let VarHandleKind::InstanceField {
        class_name,
        field_name,
        field_index,
    } = &h.kind
    {
        assert_eq!(class_name, "Foo");
        assert_eq!(field_name, "bar");
        assert_eq!(*field_index, 0);
    } else {
        panic!("expected InstanceField");
    }
}

#[test]
fn registry_create_static_field_handle() {
    let mut reg = VarHandleRegistry::new();
    let id = reg.create_static_field_handle("Baz", "count", 3, VarType::Long);
    let h = reg.get(id).unwrap();
    assert_eq!(h.var_type, VarType::Long);
    assert!(matches!(&h.kind, VarHandleKind::StaticField { .. }));
}

#[test]
fn registry_create_array_handle() {
    let mut reg = VarHandleRegistry::new();
    let id = reg.create_array_handle(VarType::Double);
    let h = reg.get(id).unwrap();
    assert_eq!(h.var_type, VarType::Double);
    assert!(matches!(&h.kind, VarHandleKind::ArrayElement { .. }));
}

#[test]
fn registry_create_byte_buffer_handle() {
    let mut reg = VarHandleRegistry::new();
    let id = reg.create_byte_buffer_handle(VarType::Int);
    let h = reg.get(id).unwrap();
    assert!(matches!(&h.kind, VarHandleKind::ByteBufferView { .. }));
}

#[test]
fn registry_lookup_by_id() {
    let mut reg = VarHandleRegistry::new();
    let id = reg.create_instance_field_handle("A", "x", 1, VarType::Short);
    assert!(reg.get(id).is_some());
    assert!(reg.get(id + 999).is_none());
}

#[test]
fn registry_is_access_mode_supported_int_cas() {
    let mut reg = VarHandleRegistry::new();
    let id = reg.create_instance_field_handle("X", "v", 0, VarType::Int);
    assert!(reg.is_access_mode_supported(id, AccessMode::CompareAndSet));
    assert!(reg.is_access_mode_supported(id, AccessMode::GetAndAdd));
    assert!(reg.is_access_mode_supported(id, AccessMode::GetAndBitwiseOr));
}

#[test]
fn registry_is_access_mode_supported_object_no_get_and_add() {
    let mut reg = VarHandleRegistry::new();
    let id = reg.create_instance_field_handle("X", "ref", 0, VarType::Object);
    assert!(reg.is_access_mode_supported(id, AccessMode::CompareAndSet));
    assert!(reg.is_access_mode_supported(id, AccessMode::GetAndSet));
    assert!(!reg.is_access_mode_supported(id, AccessMode::GetAndAdd));
    assert!(!reg.is_access_mode_supported(id, AccessMode::GetAndBitwiseOr));
}

#[test]
fn registry_count_tracking() {
    let mut reg = VarHandleRegistry::new();
    assert_eq!(reg.count(), 0);
    reg.create_instance_field_handle("A", "a", 0, VarType::Int);
    assert_eq!(reg.count(), 1);
    reg.create_array_handle(VarType::Long);
    assert_eq!(reg.count(), 2);
    reg.create_static_field_handle("B", "b", 1, VarType::Boolean);
    assert_eq!(reg.count(), 3);
}

// -- Supported modes content checks --

#[test]
fn supported_modes_int_includes_bitwise_or() {
    let modes = VarHandle::supported_modes_for(VarType::Int);
    assert!(modes.contains(&AccessMode::GetAndBitwiseOr));
    assert!(modes.contains(&AccessMode::GetAndBitwiseOrAcquire));
    assert!(modes.contains(&AccessMode::GetAndBitwiseOrRelease));
}

#[test]
fn supported_modes_object_excludes_bitwise_or() {
    let modes = VarHandle::supported_modes_for(VarType::Object);
    assert!(!modes.contains(&AccessMode::GetAndBitwiseOr));
    assert!(!modes.contains(&AccessMode::GetAndBitwiseAnd));
    assert!(!modes.contains(&AccessMode::GetAndBitwiseXor));
}

#[test]
fn supported_modes_float_includes_add_but_not_bitwise() {
    let modes = VarHandle::supported_modes_for(VarType::Float);
    assert!(modes.contains(&AccessMode::GetAndAdd));
    assert!(!modes.contains(&AccessMode::GetAndBitwiseOr));
}

#[test]
fn supported_modes_boolean_has_cas_get_and_set_and_bitwise() {
    let modes = VarHandle::supported_modes_for(VarType::Boolean);
    assert!(modes.contains(&AccessMode::CompareAndSet));
    assert!(modes.contains(&AccessMode::GetAndSet));
    assert!(!modes.contains(&AccessMode::GetAndAdd));
    // HotSpot 25 (wave 115, lane L4): `boolean` has the bitwise updates.
    assert!(modes.contains(&AccessMode::GetAndBitwiseOr));
    assert!(modes.contains(&AccessMode::GetAndBitwiseAndAcquire));
    assert!(modes.contains(&AccessMode::GetAndBitwiseXorRelease));
}

#[test]
fn supported_modes_all_types_have_basic_read_write() {
    for vt in VarType::all() {
        let modes = VarHandle::supported_modes_for(*vt);
        assert!(modes.contains(&AccessMode::Get), "{:?} missing Get", vt);
        assert!(modes.contains(&AccessMode::Set), "{:?} missing Set", vt);
        assert!(
            modes.contains(&AccessMode::GetVolatile),
            "{:?} missing GetVolatile",
            vt
        );
        assert!(
            modes.contains(&AccessMode::SetVolatile),
            "{:?} missing SetVolatile",
            vt
        );
        assert!(
            modes.contains(&AccessMode::GetAcquire),
            "{:?} missing GetAcquire",
            vt
        );
        assert!(
            modes.contains(&AccessMode::SetRelease),
            "{:?} missing SetRelease",
            vt
        );
        assert!(
            modes.contains(&AccessMode::GetOpaque),
            "{:?} missing GetOpaque",
            vt
        );
        assert!(
            modes.contains(&AccessMode::SetOpaque),
            "{:?} missing SetOpaque",
            vt
        );
    }
}

#[test]
fn supported_modes_byte_includes_bitwise() {
    let modes = VarHandle::supported_modes_for(VarType::Byte);
    assert!(modes.contains(&AccessMode::GetAndBitwiseAnd));
    assert!(modes.contains(&AccessMode::GetAndBitwiseXor));
}
