use super::*;

/// zs s2/fp2: a constant-pool slot is 12 bytes (was 24), and the `Utf8`
/// payload is still the interned `Arc<str>` (pointer-equal round trip).
#[test]
fn zs2fp2_constant_pool_entry_is_12_bytes_and_shares_the_interned_arc() {
    assert_eq!(std::mem::size_of::<CpUtf8>(), 10);
    assert_eq!(std::mem::size_of::<ConstantPoolEntry>(), 12);
    assert_eq!(std::mem::size_of::<ConstantPool>(), 24);
    assert_eq!(std::mem::size_of::<Option<ConstantPoolEntry>>(), 12);
    let interned = cratonvm_types::intern_arc("zs2fp2/Interned");
    let pool = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8(CpUtf8::from(Arc::clone(&interned))),
        ConstantPoolEntry::ClassReference { name_index: 1 },
    ]);
    let back = pool.get_utf8_arc(1).unwrap();
    assert!(Arc::ptr_eq(&back, &interned));
    assert!(Arc::ptr_eq(&pool.get_class_name_arc(2).unwrap(), &interned));
    assert_eq!(pool.get_utf8(1), Some("zs2fp2/Interned"));
    assert_eq!(pool.get_class_name(2), Some("zs2fp2/Interned"));
    let before = Arc::strong_count(&interned);
    let cloned = pool.get(1).cloned();
    assert_eq!(Arc::strong_count(&interned), before + 1);
    drop(cloned);
    drop(pool);
    assert_eq!(Arc::strong_count(&interned), before - 1);
}

/// zs s2/fp2: the spilled form (a string of 65 535 bytes or more, which no
/// class file holds) and the boundary below it answer like the inline
/// one; long / double halves round-trip every bit.
#[test]
fn zs2fp2_cp_payloads_round_trip_at_the_boundaries() {
    for n in [0usize, 1, 65_534, 65_535, 65_536, 200_000] {
        let text: String = "\u{e9}".repeat(n / 2) + &"x".repeat(n % 2);
        let arc: Arc<str> = Arc::from(text.as_str());
        let cp = CpUtf8::from(Arc::clone(&arc));
        assert_eq!(cp.as_str(), text);
        assert_eq!(&*cp.to_arc(), text.as_str());
        assert!(cp.ptr_eq_arc(&arc), "n={n}");
        assert!(Arc::ptr_eq(&cp.to_arc(), &arc), "n={n}");
        let c2 = cp.clone();
        assert_eq!(c2, cp);
        assert_eq!(format!("{c2:?}"), format!("{arc:?}"));
        drop(c2);
        drop(cp);
        assert_eq!(Arc::strong_count(&arc), 1, "n={n}");
    }
    for v in [
        0i64,
        1,
        -1,
        i64::MIN,
        i64::MAX,
        0xDEAD_BEEF_CAFE_BABEu64 as i64,
    ] {
        assert_eq!(CpLong::new(v).get(), v);
        assert_eq!(
            format!("{:?} {}", CpLong::new(v), CpLong::new(v)),
            format!("{v:?} {v}")
        );
    }
    for bits in [
        0u64,
        0x8000_0000_0000_0000,
        0x7FF8_0000_0000_0001,
        0xFFF0_0000_0000_0000,
        3.25f64.to_bits(),
    ] {
        let d = CpDouble::new(f64::from_bits(bits));
        assert_eq!(d.to_bits(), bits);
        assert_eq!(d.get().to_bits(), bits);
    }
    assert_eq!(
        format!("{:?}", CpDouble::new(2.75)),
        format!("{:?}", 2.75f64)
    );
}

#[test]
fn constant_pool_utf8_lookup() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("Hello".into()),
    ];
    let pool = ConstantPool::new(entries);
    assert_eq!(pool.get_utf8(1), Some("Hello"));
    assert_eq!(pool.get_utf8(0), None);
    assert_eq!(pool.get_utf8(99), None);
}

#[test]
fn constant_pool_class_name_resolution() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("java/lang/Object".into()),
        ConstantPoolEntry::ClassReference { name_index: 1 },
    ];
    let pool = ConstantPool::new(entries);
    assert_eq!(pool.get_class_name(2), Some("java/lang/Object"));
}

// -- Constant pool validation tests --

#[test]
fn validate_valid_pool() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("java/lang/Object".into()),
        ConstantPoolEntry::Utf8("test".into()),
        ConstantPoolEntry::Utf8("()V".into()),
        ConstantPoolEntry::ClassReference { name_index: 1 },
        ConstantPoolEntry::NameAndType {
            name_index: 2,
            descriptor_index: 3,
        },
        ConstantPoolEntry::MethodReference {
            class_index: 4,
            name_and_type_index: 5,
        },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert!(errors.is_empty(), "expected no errors, got: {:?}", errors);
}

#[test]
fn validate_class_ref_points_to_non_utf8() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Integer(42),
        ConstantPoolEntry::ClassReference { name_index: 1 }, // points to Integer, not Utf8
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("does not point to Utf8"));
}

#[test]
fn validate_class_ref_out_of_bounds() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::ClassReference { name_index: 99 },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("out of bounds"));
}

#[test]
fn validate_method_ref_class_not_class_ref() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("test".into()),
        ConstantPoolEntry::Utf8("desc".into()),
        ConstantPoolEntry::NameAndType {
            name_index: 1,
            descriptor_index: 2,
        },
        ConstantPoolEntry::MethodReference {
            class_index: 1, // points to Utf8, not ClassReference
            name_and_type_index: 3,
        },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert!(!errors.is_empty());
    assert!(errors
        .iter()
        .any(|e| e.contains("does not point to ClassReference")));
}

#[test]
fn validate_string_ref_points_to_non_utf8() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Integer(42),
        ConstantPoolEntry::StringReference { string_index: 1 },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("does not point to Utf8"));
}

#[test]
fn validate_name_and_type_wrong_types() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Integer(1),
        ConstantPoolEntry::Float(1.0),
        ConstantPoolEntry::NameAndType {
            name_index: 1,
            descriptor_index: 2,
        },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 2);
}

#[test]
fn validate_zero_index_rejected() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::ClassReference { name_index: 0 }, // index 0 is Tombstone
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("out of bounds"));
}

#[test]
fn get_name_and_type_works() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("hello".into()),
        ConstantPoolEntry::Utf8("()V".into()),
        ConstantPoolEntry::NameAndType {
            name_index: 1,
            descriptor_index: 2,
        },
    ];
    let pool = ConstantPool::new(entries);
    assert_eq!(pool.get_name_and_type(3), Some(("hello", "()V")));
    assert_eq!(pool.get_name_and_type(1), None); // Utf8, not NameAndType
}

#[test]
fn pool_len_and_empty() {
    let pool = ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
    assert_eq!(pool.len(), 1);
    assert!(pool.is_empty());

    let pool2 = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("x".into()),
    ]);
    assert_eq!(pool2.len(), 2);
    assert!(!pool2.is_empty());
}

// ---------------------------------------------------------------------
// Audit fix (LOW): validate() now covers MethodHandle / Dynamic /
// InvokeDynamic, which the old `_ => {}` arm silently skipped.
// ---------------------------------------------------------------------

#[test]
fn validate_method_handle_well_formed() {
    // reference_kind 6 (invokeStatic) -> Methodref is valid.
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("C".into()),
        ConstantPoolEntry::Utf8("m".into()),
        ConstantPoolEntry::Utf8("()V".into()),
        ConstantPoolEntry::ClassReference { name_index: 1 },
        ConstantPoolEntry::NameAndType {
            name_index: 2,
            descriptor_index: 3,
        },
        ConstantPoolEntry::MethodReference {
            class_index: 4,
            name_and_type_index: 5,
        },
        ConstantPoolEntry::MethodHandle {
            reference_kind: 6,
            reference_index: 6, // -> MethodReference
        },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
}

#[test]
fn validate_method_handle_reference_wrong_type() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Integer(7),
        ConstantPoolEntry::MethodHandle {
            reference_kind: 1,
            reference_index: 1, // -> Integer, not a Field/Method/InterfaceMethod ref
        },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].contains("does not point to a Field/Method/InterfaceMethod reference"),
        "got: {errors:?}"
    );
}

#[test]
fn validate_method_handle_reference_out_of_bounds() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::MethodHandle {
            reference_kind: 1,
            reference_index: 99,
        },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("out of bounds"), "got: {errors:?}");
}

#[test]
fn validate_dynamic_name_and_type_wrong_type() {
    // bootstrap_method_attr_index is NOT a constant-pool index, so it is
    // not checked; only name_and_type_index is. Point it at a Utf8.
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("notNaT".into()),
        ConstantPoolEntry::Dynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 1, // -> Utf8, not NameAndType
        },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].contains("does not point to NameAndType"),
        "got: {errors:?}"
    );
}

#[test]
fn validate_invoke_dynamic_well_formed() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("m".into()),
        ConstantPoolEntry::Utf8("()V".into()),
        ConstantPoolEntry::NameAndType {
            name_index: 1,
            descriptor_index: 2,
        },
        ConstantPoolEntry::InvokeDynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 3, // -> NameAndType
        },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
}

// ---------------------------------------------------------------------
// C2 hardening: Module / Package name_index (JVMS §4.4.11, §4.4.12).
// These were the last reference-bearing tags in the `_ => {}` arm.
// ---------------------------------------------------------------------

#[test]
fn validate_module_and_package_well_formed() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("java.base".into()),
        ConstantPoolEntry::Module { name_index: 1 },
        ConstantPoolEntry::Package { name_index: 1 },
    ];
    let pool = ConstantPool::new(entries);
    assert!(pool.validate().is_empty());
}

#[test]
fn validate_module_name_index_not_utf8() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Integer(7),
        ConstantPoolEntry::Module { name_index: 1 },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 1, "got: {errors:?}");
    assert!(
        errors[0].contains("Module name_index 1 does not point to Utf8"),
        "got: {errors:?}"
    );
}

#[test]
fn validate_package_name_index_zero_and_out_of_bounds() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Package { name_index: 0 },
        ConstantPoolEntry::Package { name_index: 999 },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 2, "got: {errors:?}");
    assert!(errors.iter().all(|e| e.contains("Package name_index")));
    assert!(errors.iter().all(|e| e.contains("out of bounds")));
}

/// A `Module` whose `name_index` lands on the *second slot* of a
/// `CONSTANT_Long` must be rejected. That slot is a `Tombstone`, which
/// is neither `Utf8` nor out of bounds — it is the distinct
/// "unusable second half of a category-2 entry" hazard from JVMS §4.4.5.
#[test]
fn validate_rejects_index_into_the_second_slot_of_a_long() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Long(1.into()),
        ConstantPoolEntry::Tombstone, // the unusable second slot
        ConstantPoolEntry::Module { name_index: 2 },
        ConstantPoolEntry::ClassReference { name_index: 2 },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 2, "got: {errors:?}");
    assert!(errors.iter().all(|e| e.contains("does not point to Utf8")));
}

#[test]
fn validate_invoke_dynamic_out_of_bounds() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::InvokeDynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 0, // index 0 is Tombstone -> rejected
        },
    ];
    let pool = ConstantPool::new(entries);
    let errors = pool.validate();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("out of bounds"), "got: {errors:?}");
}
