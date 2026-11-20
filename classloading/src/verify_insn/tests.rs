use super::super::verify_frame::VerificationFrame;
use super::super::vtype::ClassHierarchy;
use super::*;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

struct MockHierarchy;

impl ClassHierarchy for MockHierarchy {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        if child == parent || parent == "java/lang/Object" {
            return true;
        }
        // Model the JDK exception hierarchy so the `athrow` operand
        // check (operand must be assignable to java/lang/Throwable)
        // can be exercised by the tests.
        matches!(
            (child, parent),
            ("java/lang/Exception", "java/lang/Throwable")
                | ("java/lang/RuntimeException", "java/lang/Throwable")
                | ("java/lang/RuntimeException", "java/lang/Exception")
                | ("java/lang/Error", "java/lang/Throwable")
        )
    }
    fn common_superclass(&self, _a: &str, _b: &str) -> String {
        "java/lang/Object".to_string()
    }
    fn is_interface(&self, _name: &str) -> bool {
        false
    }
}

/// Interpreter round i5 wave 101, lane L7: HotSpot's receiver rule.
#[test]
fn receiver_rule_refuses_an_unassignable_loaded_receiver() {
    struct Unloaded;
    impl ClassHierarchy for Unloaded {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            child == parent || parent == "java/lang/Object"
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
        fn is_resolvable(&self, name: &str) -> bool {
            name != "p/NotLoaded"
        }
    }
    let h = MockHierarchy;
    let mut scratch = VerifyScratch::for_class_version(52);
    scratch.receiver_rule = true;
    let mut frame = make_frame(1, 4);
    let class = VType::ObjectRef(Arc::from("java/lang/Class"));
    let err = check_receiver_assignable(
        &mut frame,
        &mut scratch,
        &class,
        "java/lang/String",
        &h,
        "Bad type on operand stack",
        "invokevirtual",
        None,
    )
    .expect_err("a Class is not a String");
    assert!(err.to_string().contains("invokevirtual"), "{err}");
    assert!(matches!(
        frame.take_fault(),
        Some(VerifyFault::BadTypeWith { index: 0, .. })
    ));
    // Cached: the same answer the second time.
    assert!(check_receiver_assignable(
        &mut frame,
        &mut scratch,
        &class,
        "java/lang/String",
        &h,
        "Bad type on operand stack",
        "invokevirtual",
        None,
    )
    .is_err());
    // Object targets, equal names, null and subclasses pass.
    for (recv, target) in [
        (class.clone(), "java/lang/Object"),
        (class.clone(), "java/lang/Class"),
        (VType::Null, "java/lang/String"),
        (VType::ObjectRef(Arc::from("java/lang/RuntimeException")), "java/lang/Exception"),
    ] {
        assert!(check_receiver_assignable(
            &mut frame, &mut scratch, &recv, target, &h, "h", "w", None
        )
        .is_ok());
    }
    // A pair the hierarchy cannot decide (a class not loaded yet) passes:
    // HotSpot would load it, this verifier cannot.
    let unloaded = VType::ObjectRef(Arc::from("p/NotLoaded"));
    assert!(check_receiver_assignable(
        &mut frame,
        &mut scratch,
        &unloaded,
        "java/lang/String",
        &Unloaded,
        "h",
        "w",
        None
    )
    .is_ok());
    // Switched off (`CRATONVM_VERIFY_RECEIVER_ASSIGNABLE=0`): no check.
    let mut off = VerifyScratch::default();
    assert!(check_receiver_assignable(
        &mut frame, &mut off, &class, "java/lang/String", &h, "h", "w", None
    )
    .is_ok());
}

fn simple_cp() -> ConstantPool {
    ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,                        // 0
        ConstantPoolEntry::Integer(42),                      // 1
        ConstantPoolEntry::Float(3.125),                     // 2
        ConstantPoolEntry::Long(100.into()),                 // 3
        ConstantPoolEntry::Tombstone,                        // 4 (second slot of long)
        ConstantPoolEntry::Double(2.75.into()),              // 5
        ConstantPoolEntry::Tombstone,                        // 6 (second slot of double)
        ConstantPoolEntry::Utf8("java/lang/Object".into()),  // 7
        ConstantPoolEntry::ClassReference { name_index: 7 }, // 8
        ConstantPoolEntry::Utf8("value".into()),             // 9
        ConstantPoolEntry::Utf8("I".into()),                 // 10
        ConstantPoolEntry::NameAndType {
            name_index: 9,
            descriptor_index: 10,
        }, // 11
        ConstantPoolEntry::FieldReference {
            class_index: 8,
            name_and_type_index: 11,
        }, // 12
        ConstantPoolEntry::Utf8("toString".into()),          // 13
        ConstantPoolEntry::Utf8("()Ljava/lang/String;".into()), // 14
        ConstantPoolEntry::NameAndType {
            name_index: 13,
            descriptor_index: 14,
        }, // 15
        ConstantPoolEntry::MethodReference {
            class_index: 8,
            name_and_type_index: 15,
        }, // 16
        ConstantPoolEntry::StringReference { string_index: 7 }, // 17
    ])
}

fn make_frame(max_locals: u16, max_stack: u16) -> VerificationFrame {
    VerificationFrame::initial_frame("Test", "test", "()V", true, max_locals, max_stack)
}

#[test]
fn verify_iconst_pushes_int() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    let result = verify_instruction(
        &Instruction::Iconst0,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert!(result.falls_through);
    assert!(result.branch_targets.is_empty());
    assert_eq!(frame.stack_depth(), 1);
    assert_eq!(frame.pop().unwrap(), VType::Int);
}

#[test]
fn verify_iadd_type_effect() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    frame.push(VType::Int).unwrap();
    frame.push(VType::Int).unwrap();

    verify_instruction(
        &Instruction::Iadd,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert_eq!(frame.stack_depth(), 1);
    assert_eq!(frame.pop().unwrap(), VType::Int);
}

#[test]
fn verify_iadd_wrong_types_fails() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    frame.push(VType::Float).unwrap();
    frame.push(VType::Int).unwrap();

    // Second pop expects Int but finds Float
    let result = verify_instruction(
        &Instruction::Iadd,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    );
    assert!(result.is_err());
}

#[test]
fn verify_ldc_integer() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    verify_instruction(
        &Instruction::Ldc(1),
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert_eq!(frame.pop().unwrap(), VType::Int);
}

#[test]
fn verify_ldc_string() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    verify_instruction(
        &Instruction::Ldc(17),
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert_eq!(
        frame.pop().unwrap(),
        VType::ObjectRef(Arc::from("java/lang/String"))
    );
}

/// libverify loads a class literal with `ldc` only from major 49
/// (interpreter round i5 wave 109, lane L7).
#[test]
fn ldc_class_is_refused_below_major_49() {
    let cp = simple_cp();
    let run = |major: u16| {
        let mut frame = make_frame(1, 4);
        let mut scratch = VerifyScratch::for_class_version(major);
        let result = verify_instruction_with(
            &Instruction::Ldc(8),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &MockHierarchy,
            &mut scratch,
        );
        (result.is_ok(), frame.take_fault())
    };
    let (ok, fault) = run(48);
    assert!(!ok);
    assert!(matches!(fault, Some(VerifyFault::BadCpIndex { index: 8, .. })));
    assert!(run(49).0);
    assert!(run(69).0);
    // A map-only walk keeps its maps.
    let mut frame = make_frame(1, 4);
    let mut scratch = VerifyScratch::for_class_version(48).without_linkage_rules();
    assert!(verify_instruction_with(
        &Instruction::Ldc(8),
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &MockHierarchy,
        &mut scratch,
    )
    .is_ok());
}

/// HotSpot `verify_switch`: `lookupswitch` keys must be strictly
/// increasing (interpreter round i5 wave 109, lane L7).
#[test]
fn unsorted_lookupswitch_is_refused() {
    let cp = simple_cp();
    let run = |pairs: Vec<(i32, i32)>| {
        let mut frame = make_frame(1, 4);
        frame.push(VType::Int).unwrap();
        let insn = Instruction::Lookupswitch(Arc::new(
            cratonvm_reader::instruction::LookupSwitch { default: 3, pairs },
        ));
        let result = verify_instruction_with(
            &insn,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &MockHierarchy,
            &mut VerifyScratch::for_class_version(69),
        );
        (result.is_ok(), frame.take_fault())
    };
    assert!(run(vec![(3, 3), (5, 3)]).0);
    for pairs in [vec![(5, 3), (3, 3)], vec![(3, 3), (3, 3)]] {
        let (ok, fault) = run(pairs);
        assert!(!ok);
        assert!(matches!(
            fault,
            Some(VerifyFault::BadCode { header }) if header == "Bad lookupswitch instruction"
        ));
    }
}

/// Constant pool with two Dynamic constants for the ldc/ldc2_w
/// category tests: index #3 is a category-1 Dynamic (`I`), index #6 is
/// a category-2 Dynamic (`J`).
fn dynamic_cp() -> ConstantPool {
    ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,        // 0
        ConstantPoolEntry::Utf8("d".into()), // 1
        ConstantPoolEntry::Utf8("I".into()), // 2 (category-1 descriptor)
        ConstantPoolEntry::NameAndType {
            name_index: 1,
            descriptor_index: 2,
        }, // 3 (NameAndType for cat-1)
        ConstantPoolEntry::Dynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 3,
        }, // 4 (category-1 Dynamic)
        ConstantPoolEntry::Utf8("J".into()), // 5 (category-2 descriptor)
        ConstantPoolEntry::NameAndType {
            name_index: 1,
            descriptor_index: 5,
        }, // 6 (NameAndType for cat-2)
        ConstantPoolEntry::Dynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 6,
        }, // 7 (category-2 Dynamic)
    ])
}

#[test]
fn ldc_of_category2_dynamic_is_rejected() {
    // SPEC-COMPLIANCE FIX (MED): `ldc` of a Dynamic constant whose type
    // is category-2 (long/double) must be rejected — pushing it without
    // the paired `Top` would corrupt the verifier stack model.
    let cp = dynamic_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    let res = verify_instruction(
        &Instruction::Ldc(7), // category-2 Dynamic
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    );
    assert!(
        res.is_err(),
        "ldc of a category-2 Dynamic constant must be rejected, got {res:?}"
    );
    // The frame must be untouched on rejection (no half-pushed cat-2).
    assert_eq!(frame.stack_depth(), 0);
}

#[test]
fn ldc_of_category1_dynamic_is_accepted() {
    // A category-1 Dynamic constant is the legal `ldc` case: pushes a
    // single slot, no Top.
    let cp = dynamic_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    verify_instruction(
        &Instruction::Ldc(4), // category-1 Dynamic (`I`)
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .expect("ldc of a category-1 Dynamic must verify");
    assert_eq!(frame.stack_depth(), 1);
    assert_eq!(frame.pop().unwrap(), VType::Int);
}

#[test]
fn ldc2w_of_category2_dynamic_pushes_top_slot() {
    // `ldc2_w` is the correct loader for a category-2 Dynamic constant:
    // it must push the Long/Double base plus its paired Top upper half.
    let cp = dynamic_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    verify_instruction(
        &Instruction::Ldc2W(7), // category-2 Dynamic (`J`)
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .expect("ldc2_w of a category-2 Dynamic must verify");
    assert_eq!(frame.stack_depth(), 2);
    assert_eq!(frame.pop().unwrap(), VType::Top);
    assert_eq!(frame.pop().unwrap(), VType::Long);
}

#[test]
fn ldc2w_of_category1_dynamic_is_rejected() {
    // A category-1 Dynamic under `ldc2_w` is ill-formed (belongs under
    // `ldc`) and must be rejected.
    let cp = dynamic_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    let res = verify_instruction(
        &Instruction::Ldc2W(4), // category-1 Dynamic (`I`)
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    );
    assert!(
        res.is_err(),
        "ldc2_w of a category-1 Dynamic constant must be rejected, got {res:?}"
    );
    assert_eq!(frame.stack_depth(), 0);
}

#[test]
fn verify_getstatic() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    // getstatic index=12 → field "value" of type "I"
    verify_instruction(
        &Instruction::Getstatic(12),
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert_eq!(frame.pop().unwrap(), VType::Int);
}

#[test]
fn verify_goto_no_fallthrough() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    let result = verify_instruction(
        &Instruction::Goto(10),
        5,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert!(!result.falls_through);
    assert_eq!(result.branch_targets, vec![15]);
}

#[test]
fn verify_ifeq_branch() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);
    frame.push(VType::Int).unwrap();

    let result = verify_instruction(
        &Instruction::Ifeq(20),
        5,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert!(result.falls_through);
    assert_eq!(result.branch_targets, vec![25]);
    assert_eq!(frame.stack_depth(), 0);
}

#[test]
fn verify_return_no_fallthrough() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    let result = verify_instruction(
        &Instruction::Return,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert!(!result.falls_through);
}

#[test]
fn verify_new_pushes_uninitialized() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    // new at PC=10, class index=8 (Object)
    verify_instruction(
        &Instruction::New(8),
        10,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert_eq!(frame.pop().unwrap(), VType::Uninitialized(10));
}

#[test]
fn verify_aconst_null_pushes_null() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);

    verify_instruction(
        &Instruction::AconstNull,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert_eq!(frame.pop().unwrap(), VType::Null);
}

#[test]
fn verify_dup() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);
    frame.push(VType::Int).unwrap();

    verify_instruction(
        &Instruction::Dup,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert_eq!(frame.stack_depth(), 2);
}

#[test]
fn verify_athrow_no_fallthrough() {
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);
    frame
        .push(VType::ObjectRef(Arc::from("java/lang/Exception")))
        .unwrap();

    let result = verify_instruction(
        &Instruction::Athrow,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();

    assert!(!result.falls_through);
}

// --- RVERIF.3 soundness regressions -----------------------------------

#[test]
fn athrow_rejects_non_throwable() {
    // A reference that is not assignable to java/lang/Throwable must
    // be rejected by athrow (JVMS §6.5 athrow).
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);
    frame
        .push(VType::ObjectRef(Arc::from("java/lang/String")))
        .unwrap();

    let result = verify_instruction(
        &Instruction::Athrow,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    );
    assert!(result.is_err(), "athrow of a non-Throwable must fail");
}

#[test]
fn athrow_accepts_null() {
    // `athrow null` is legal bytecode (raises NPE at runtime).
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);
    frame.push(VType::Null).unwrap();

    verify_instruction(
        &Instruction::Athrow,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .unwrap();
}

#[test]
fn areturn_rejects_unassignable_return() {
    // Method declared to return java/lang/String must reject an
    // areturn of an unrelated reference type (JVMS §6.5 areturn).
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);
    frame
        .push(VType::ObjectRef(Arc::from("java/lang/Thread")))
        .unwrap();

    let result = verify_instruction(
        &Instruction::Areturn,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()Ljava/lang/String;",
        &h,
    );
    assert!(
        result.is_err(),
        "areturn of a type not assignable to the declared return type must fail"
    );
}

#[test]
fn areturn_accepts_assignable_return() {
    // Returning the declared type itself succeeds.
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);
    frame
        .push(VType::ObjectRef(Arc::from("java/lang/String")))
        .unwrap();

    verify_instruction(
        &Instruction::Areturn,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()Ljava/lang/String;",
        &h,
    )
    .unwrap();
}

#[test]
fn areturn_rejects_void_method() {
    // areturn must not appear in a method declared void.
    let cp = simple_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);
    frame.push(VType::Null).unwrap();

    let result = verify_instruction(
        &Instruction::Areturn,
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    );
    assert!(result.is_err(), "areturn in a void method must fail");
}

/// Build a constant pool whose entry #6 is a `MethodReference` to
/// `java/lang/Object.<init>()V`, for `invokespecial <init>` tests.
fn init_cp() -> ConstantPool {
    ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,                        // 0
        ConstantPoolEntry::Utf8("java/lang/Object".into()),  // 1
        ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
        ConstantPoolEntry::Utf8("<init>".into()),            // 3
        ConstantPoolEntry::Utf8("()V".into()),               // 4
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        ConstantPoolEntry::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6
    ])
}

#[test]
fn invokespecial_init_on_initialized_ref_rejected() {
    // JVMS §4.10.1.9: `<init>` may only be invoked on an *uninitialized*
    // receiver. An already-initialized ObjectRef must be rejected.
    let cp = init_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);
    frame
        .push(VType::ObjectRef(Arc::from("java/lang/Object")))
        .unwrap();

    let result = verify_instruction(
        &Instruction::Invokespecial(6),
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    );
    assert!(
        result.is_err(),
        "invokespecial <init> on an initialized ObjectRef must fail verification"
    );
}

#[test]
fn invokespecial_init_on_null_rejected() {
    // `Null` is a reference but not an uninitialized object — reject.
    let cp = init_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);
    frame.push(VType::Null).unwrap();

    let result = verify_instruction(
        &Instruction::Invokespecial(6),
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    );
    assert!(
        result.is_err(),
        "invokespecial <init> on Null must fail verification"
    );
}

#[test]
fn invokespecial_init_on_uninitialized_initializes_slot() {
    // A `Uninitialized(offset)` receiver is legal; after the call every
    // aliased copy of that uninitialized type becomes the owner ObjectRef.
    let cp = init_cp();
    let h = MockHierarchy;
    let mut frame = make_frame(1, 4);
    // local 0 aliases the same uninitialized object (as if `dup`'d into it)
    frame.locals[0] = VType::Uninitialized(0);
    frame.push(VType::Uninitialized(0)).unwrap();

    verify_instruction(
        &Instruction::Invokespecial(6),
        0,
        &mut frame,
        &cp,
        "Test",
        "test",
        "()V",
        &h,
    )
    .expect("invokespecial <init> on an uninitialized receiver must verify");

    // The receiver was popped; the aliased local must now be initialized.
    assert_eq!(
        frame.locals[0],
        VType::ObjectRef(Arc::from("java/lang/Object")),
        "uninitialized slot must be replaced with the initialized owner type"
    );
}

#[test]
fn invokespecial_init_on_uninitialized_this_accepts_direct_superclass() {
    struct DirectSuperclassHierarchy;

    impl ClassHierarchy for DirectSuperclassHierarchy {
        fn is_subclass(&self, _child: &str, _parent: &str) -> bool {
            false
        }

        fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
            child == "Example" && parent == "java/lang/Object"
        }

        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }

        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    let cp = init_cp();
    let h = DirectSuperclassHierarchy;
    let mut frame = make_frame(1, 4);
    frame.locals[0] = VType::UninitializedThis;
    frame.push(VType::UninitializedThis).unwrap();

    verify_instruction(
        &Instruction::Invokespecial(6),
        0,
        &mut frame,
        &cp,
        "Example",
        "<init>",
        "()V",
        &h,
    )
    .expect("uninitializedThis must be allowed to invoke its direct superclass constructor");

    assert_eq!(frame.locals[0], VType::ObjectRef(Arc::from("Example")));
}

#[test]
fn invokespecial_init_on_uninitialized_this_rejects_indirect_superclass() {
    struct IndirectSuperclassHierarchy;

    impl ClassHierarchy for IndirectSuperclassHierarchy {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            child == "Example" && parent == "java/lang/Object"
        }

        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }

        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    let cp = init_cp();
    let h = IndirectSuperclassHierarchy;
    let mut frame = make_frame(1, 4);
    frame.locals[0] = VType::UninitializedThis;
    frame.push(VType::UninitializedThis).unwrap();

    let error = verify_instruction(
        &Instruction::Invokespecial(6),
        0,
        &mut frame,
        &cp,
        "Example",
        "<init>",
        "()V",
        &h,
    )
    .expect_err("uninitializedThis must not invoke an indirect superclass constructor");

    assert!(error
        .to_string()
        .contains("uninitializedThis receiver requires the constructor owner"));
}

// =====================================================================
// Constant-pool cross-checks (JVMS §4.9.1)
// =====================================================================

fn run(
    insn: &Instruction,
    frame: &mut VerificationFrame,
    cp: &ConstantPool,
    class: &str,
    method: &str,
    descriptor: &str,
) -> Result<InsnVerifyResult, LinkageError> {
    verify_instruction(
        insn,
        0,
        frame,
        cp,
        class,
        method,
        descriptor,
        &MockHierarchy,
    )
}

#[test]
fn new_with_a_valid_class_entry_is_accepted() {
    let cp = init_cp(); // index 2 is a ClassReference → java/lang/Object
    let mut frame = make_frame(1, 4);
    run(&Instruction::New(2), &mut frame, &cp, "Test", "m", "()V")
        .expect("`new java/lang/Object` is well-formed");
    assert_eq!(frame.stack, vec![VType::Uninitialized(0)]);
}

#[test]
fn new_with_a_non_class_cp_entry_rejected() {
    // Index 4 is a Utf8, not a CONSTANT_Class. The operand used to be
    // ignored entirely, so `new #4` verified clean.
    let cp = init_cp();
    let mut frame = make_frame(1, 4);
    let err = run(&Instruction::New(4), &mut frame, &cp, "Test", "m", "()V")
        .expect_err("`new` must reject a non-Class operand");
    assert!(err.to_string().contains("CONSTANT_Class"), "{err}");
}

#[test]
fn new_with_an_out_of_range_cp_index_rejected() {
    let cp = init_cp();
    let mut frame = make_frame(1, 4);
    assert!(run(&Instruction::New(9999), &mut frame, &cp, "Test", "m", "()V").is_err());
}

#[test]
fn new_of_an_array_type_rejected() {
    // JVMS §6.5 new: array types are created by anewarray / multianewarray.
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("[Ljava/lang/Object;".into()), // 1
        ConstantPoolEntry::ClassReference { name_index: 1 },   // 2
    ]);
    let mut frame = make_frame(1, 4);
    let err = run(&Instruction::New(2), &mut frame, &cp, "Test", "m", "()V")
        .expect_err("`new` of an array type must be rejected");
    assert!(err.to_string().contains("array type"), "{err}");
}

#[test]
fn instanceof_with_a_non_class_cp_entry_rejected() {
    let cp = init_cp();
    let mut frame = make_frame(1, 4);
    frame.push(VType::Null).unwrap();
    assert!(run(
        &Instruction::Instanceof(4),
        &mut frame,
        &cp,
        "Test",
        "m",
        "()V"
    )
    .is_err());
}

#[test]
fn instanceof_with_a_valid_class_entry_accepted() {
    let cp = init_cp();
    let mut frame = make_frame(1, 4);
    frame.push(VType::Null).unwrap();
    run(
        &Instruction::Instanceof(2),
        &mut frame,
        &cp,
        "Test",
        "m",
        "()V",
    )
    .expect("`instanceof java/lang/Object` is well-formed");
    assert_eq!(frame.stack, vec![VType::Int]);
}

#[test]
fn getstatic_with_a_malformed_field_descriptor_rejected() {
    // A descriptor `VType::from_field_descriptor` cannot parse becomes
    // `Top`, and everything is assignable to `Top` — so a malformed
    // descriptor used to disable the type check on this operand entirely.
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("Test".into()), // 1
        ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
        ConstantPoolEntry::Utf8("f".into()),    // 3
        ConstantPoolEntry::Utf8("Ljava/lang/String".into()), // 4 — no ';'
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        ConstantPoolEntry::FieldReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6
    ]);
    let mut frame = make_frame(1, 4);
    let err = run(
        &Instruction::Getstatic(6),
        &mut frame,
        &cp,
        "Test",
        "m",
        "()V",
    )
    .expect_err("a malformed field descriptor must be a VerifyError");
    assert!(err.to_string().contains("field descriptor"), "{err}");
}

#[test]
fn getstatic_with_a_well_formed_field_descriptor_accepted() {
    let cp = simple_cp(); // index 12 is Test.value : I
    let mut frame = make_frame(1, 4);
    run(
        &Instruction::Getstatic(12),
        &mut frame,
        &cp,
        "Test",
        "m",
        "()V",
    )
    .expect("a well-formed field ref must verify");
    assert_eq!(frame.stack, vec![VType::Int]);
}

#[test]
fn invokestatic_with_a_malformed_method_descriptor_rejected() {
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("Test".into()), // 1
        ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
        ConstantPoolEntry::Utf8("m".into()),    // 3
        ConstantPoolEntry::Utf8("(Ljava/lang/String)V".into()), // 4 — no ';'
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        ConstantPoolEntry::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6
    ]);
    let mut frame = make_frame(1, 4);
    let err = run(
        &Instruction::Invokestatic(6),
        &mut frame,
        &cp,
        "Test",
        "m",
        "()V",
    )
    .expect_err("a malformed method descriptor must be a VerifyError");
    assert!(err.to_string().contains("method descriptor"), "{err}");
}

// =====================================================================
// Category-2 local slot pairing (JVMS §4.10.1.6)
// =====================================================================

#[test]
fn lstore_then_lload_round_trips() {
    let cp = simple_cp();
    let mut frame = make_frame(4, 4);
    frame.push(VType::Long).unwrap();
    frame.push(VType::Top).unwrap();
    run(&Instruction::Lstore(1), &mut frame, &cp, "Test", "m", "()V").unwrap();
    run(&Instruction::Lload(1), &mut frame, &cp, "Test", "m", "()V")
        .expect("an intact long pair must load back");
    assert_eq!(frame.stack, vec![VType::Long, VType::Top]);
}

#[test]
fn splitting_a_long_pair_then_loading_it_rejected() {
    // `lstore_1; istore_2; lload_1` — the `istore_2` overwrote the long's
    // upper half, so `lload_1` must not verify.
    let cp = simple_cp();
    let mut frame = make_frame(4, 4);
    frame.push(VType::Long).unwrap();
    frame.push(VType::Top).unwrap();
    run(&Instruction::Lstore(1), &mut frame, &cp, "Test", "m", "()V").unwrap();
    frame.push(VType::Int).unwrap();
    run(&Instruction::Istore(2), &mut frame, &cp, "Test", "m", "()V").unwrap();
    let err = run(&Instruction::Lload(1), &mut frame, &cp, "Test", "m", "()V")
        .expect_err("a split category-2 pair must not load back as a long");
    assert!(err.to_string().contains("lload"), "{err}");
}

#[test]
fn lstore_past_max_locals_rejected() {
    // `max_locals = 2` leaves slots 0 and 1; a long at slot 1 needs slot 2.
    let cp = simple_cp();
    let mut frame = make_frame(2, 4);
    frame.push(VType::Long).unwrap();
    frame.push(VType::Top).unwrap();
    let err = run(&Instruction::Lstore(1), &mut frame, &cp, "Test", "m", "()V")
        .expect_err("a long store must fit entirely inside max_locals");
    assert!(err.to_string().contains("lstore"), "{err}");
}

#[test]
fn wide_lstore_at_u16_max_does_not_panic() {
    // REGRESSION: `local_store(*index + 1, ..)` overflowed for a
    // `wide lstore 65535` — a debug panic on attacker-supplied bytecode.
    let cp = simple_cp();
    let mut frame = make_frame(4, 4);
    frame.push(VType::Long).unwrap();
    frame.push(VType::Top).unwrap();
    assert!(run(
        &Instruction::Lstore(u16::MAX),
        &mut frame,
        &cp,
        "Test",
        "m",
        "()V"
    )
    .is_err());
}

// =====================================================================
// `<init>` must run before a constructor returns (JVMS §4.10.1.9)
// =====================================================================

#[test]
fn constructor_return_before_super_init_rejected() {
    let cp = init_cp();
    let mut frame = make_frame(1, 4);
    frame.locals[0] = VType::UninitializedThis;
    let err = run(
        &Instruction::Return,
        &mut frame,
        &cp,
        "Example",
        "<init>",
        "()V",
    )
    .expect_err("a constructor may not return with `this` uninitialized");
    assert!(err.to_string().contains("uninitialized"), "{err}");
}

#[test]
fn constructor_return_after_super_init_accepted() {
    struct DirectSuper;
    impl ClassHierarchy for DirectSuper {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            child == parent || parent == "java/lang/Object"
        }
        fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
            child == "Example" && parent == "java/lang/Object"
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    let cp = init_cp();
    let h = DirectSuper;
    let mut frame = make_frame(1, 4);
    frame.locals[0] = VType::UninitializedThis;
    frame.push(VType::UninitializedThis).unwrap();
    verify_instruction(
        &Instruction::Invokespecial(6),
        0,
        &mut frame,
        &cp,
        "Example",
        "<init>",
        "()V",
        &h,
    )
    .expect("super() call");
    verify_instruction(
        &Instruction::Return,
        4,
        &mut frame,
        &cp,
        "Example",
        "<init>",
        "()V",
        &h,
    )
    .expect("returning after super() is legal");
}

#[test]
fn object_constructor_may_return_uninitialized() {
    // `java/lang/Object.<init>` has no superclass to chain to, so its
    // `uninitializedThis` is never replaced.
    let cp = init_cp();
    let mut frame = make_frame(1, 4);
    frame.locals[0] = VType::UninitializedThis;
    run(
        &Instruction::Return,
        &mut frame,
        &cp,
        "java/lang/Object",
        "<init>",
        "()V",
    )
    .expect("Object.<init> must be allowed to return");
}

#[test]
fn ordinary_method_return_unaffected() {
    let cp = init_cp();
    let mut frame = make_frame(1, 4);
    run(&Instruction::Return, &mut frame, &cp, "Example", "m", "()V")
        .expect("an ordinary void return is unaffected");
}

// -----------------------------------------------------------------
// JIT round 20 (lane verif): JVMS §4.10.1.8 protected receiver and
// §4.10.1.9 invokespecial target rules.
// -----------------------------------------------------------------

/// `q/Cur extends p/Holder implements q/J`; `q/J extends q/I`; `p/Holder`
/// declares protected `protF:I` and `prot()I` and public `pub()I`;
/// `q/Sub extends q/Cur`; `p/Sib extends p/Holder`.
struct RuleHierarchy;

impl ClassHierarchy for RuleHierarchy {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        child == parent
            || parent == "java/lang/Object"
            || matches!(
                (child, parent),
                ("q/Cur", "p/Holder")
                    | ("q/Sub", "q/Cur")
                    | ("q/Sub", "p/Holder")
                    | ("p/Sib", "p/Holder")
            )
    }
    fn common_superclass(&self, _a: &str, _b: &str) -> String {
        "java/lang/Object".to_string()
    }
    fn is_interface(&self, name: &str) -> bool {
        name == "q/I" || name == "q/J"
    }
    fn invokespecial_target(
        &self,
        current: &str,
        ref_class: &str,
    ) -> Option<super::super::vtype::SpecialTarget> {
        use super::super::vtype::SpecialTarget;
        assert_eq!(current, "q/Cur");
        match ref_class {
            "p/Holder" | "q/J" => Some(SpecialTarget::SameOrDirect),
            "q/I" => Some(SpecialTarget::Assignable),
            "p/Other" => Some(SpecialTarget::NotAssignable),
            _ => None,
        }
    }
    fn protected_receiver_rule(
        &self,
        current: &str,
        ref_class: &str,
        name: &str,
        _descriptor: &str,
        _is_method: bool,
    ) -> bool {
        current == "q/Cur" && ref_class == "p/Holder" && (name == "protF" || name == "prot")
    }
}

/// 1 Utf8 p/Holder, 2 Class #1, 3 Utf8 protF, 4 Utf8 I, 5 NaT protF:I,
/// 6 Fieldref p/Holder.protF:I, 7 Utf8 prot, 8 Utf8 ()I, 9 NaT prot()I,
/// 10 Methodref p/Holder.prot()I, 11 Utf8 q/I, 12 Class #11,
/// 13 Utf8 m, 14 NaT m()I, 15 InterfaceMethodref q/I.m()I,
/// 16 Utf8 q/J, 17 Class #16, 18 InterfaceMethodref q/J.m()I,
/// 19 Utf8 p/Other, 20 Class #19, 21 Methodref p/Other.m()I,
/// 22 Utf8 pub, 23 NaT pub()I, 24 Methodref p/Holder.pub()I.
fn rule_cp() -> ConstantPool {
    ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("p/Holder".into()),
        ConstantPoolEntry::ClassReference { name_index: 1 },
        ConstantPoolEntry::Utf8("protF".into()),
        ConstantPoolEntry::Utf8("I".into()),
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        },
        ConstantPoolEntry::FieldReference {
            class_index: 2,
            name_and_type_index: 5,
        },
        ConstantPoolEntry::Utf8("prot".into()),
        ConstantPoolEntry::Utf8("()I".into()),
        ConstantPoolEntry::NameAndType {
            name_index: 7,
            descriptor_index: 8,
        },
        ConstantPoolEntry::MethodReference {
            class_index: 2,
            name_and_type_index: 9,
        },
        ConstantPoolEntry::Utf8("q/I".into()),
        ConstantPoolEntry::ClassReference { name_index: 11 },
        ConstantPoolEntry::Utf8("m".into()),
        ConstantPoolEntry::NameAndType {
            name_index: 13,
            descriptor_index: 8,
        },
        ConstantPoolEntry::InterfaceMethodReference {
            class_index: 12,
            name_and_type_index: 14,
        },
        ConstantPoolEntry::Utf8("q/J".into()),
        ConstantPoolEntry::ClassReference { name_index: 16 },
        ConstantPoolEntry::InterfaceMethodReference {
            class_index: 17,
            name_and_type_index: 14,
        },
        ConstantPoolEntry::Utf8("p/Other".into()),
        ConstantPoolEntry::ClassReference { name_index: 19 },
        ConstantPoolEntry::MethodReference {
            class_index: 20,
            name_and_type_index: 14,
        },
        ConstantPoolEntry::Utf8("pub".into()),
        ConstantPoolEntry::NameAndType {
            name_index: 22,
            descriptor_index: 8,
        },
        ConstantPoolEntry::MethodReference {
            class_index: 2,
            name_and_type_index: 23,
        },
    ])
}

fn rule_message(result: Result<InsnVerifyResult, LinkageError>) -> String {
    match result {
        Err(LinkageError::VerifyError { message, .. }) => message,
        Err(other) => panic!("expected a VerifyError, got {other:?}"),
        Ok(_) => panic!("expected a VerifyError, the instruction verified"),
    }
}

fn run_rule(
    insn: Instruction,
    receiver: VType,
    scratch: &mut VerifyScratch,
) -> Result<InsnVerifyResult, LinkageError> {
    let cp = rule_cp();
    let mut frame = make_frame(1, 4);
    frame.push(receiver).unwrap();
    verify_instruction_with(
        &insn,
        0,
        &mut frame,
        &cp,
        "q/Cur",
        "run",
        "()I",
        &RuleHierarchy,
        scratch,
    )
}

#[test]
fn protected_getfield_through_a_sibling_receiver_is_refused() {
    let mut scratch = VerifyScratch::for_class_version(69);
    let result = run_rule(
        Instruction::Getfield(6),
        VType::ObjectRef(Arc::from("p/Sib")),
        &mut scratch,
    );
    assert_eq!(
        rule_message(result),
        "Bad access to protected data in getfield"
    );
    assert!(scratch.take_linkage_rule_violation());
    assert!(!scratch.take_linkage_rule_violation());
}

#[test]
fn protected_getfield_through_own_null_or_subclass_receiver_verifies() {
    for receiver in [
        VType::ObjectRef(Arc::from("q/Cur")),
        VType::ObjectRef(Arc::from("q/Sub")),
        VType::Null,
    ] {
        let mut scratch = VerifyScratch::for_class_version(69);
        if let Err(e) = run_rule(Instruction::Getfield(6), receiver.clone(), &mut scratch) {
            panic!("{receiver:?} must verify: {e:?}");
        }
        assert!(!scratch.take_linkage_rule_violation());
    }
}

#[test]
fn protected_invokevirtual_through_a_sibling_receiver_is_refused() {
    let mut scratch = VerifyScratch::for_class_version(69);
    let result = run_rule(
        Instruction::Invokevirtual(10),
        VType::ObjectRef(Arc::from("p/Sib")),
        &mut scratch,
    );
    assert_eq!(
        rule_message(result),
        "Bad access to protected data in invokevirtual"
    );
    // A public member of the same superclass is not protected access.
    let mut scratch = VerifyScratch::for_class_version(69);
    assert!(run_rule(
        Instruction::Invokevirtual(24),
        VType::ObjectRef(Arc::from("p/Sib")),
        &mut scratch,
    )
    .is_ok());
}

#[test]
fn protected_rule_uses_the_old_verifier_text_and_is_off_for_map_walks() {
    let mut scratch = VerifyScratch::for_class_version(49);
    let result = run_rule(
        Instruction::Getfield(6),
        VType::ObjectRef(Arc::from("p/Sib")),
        &mut scratch,
    );
    assert_eq!(rule_message(result), "Bad access to protected data");
    let mut scratch = VerifyScratch::for_class_version(69).without_linkage_rules();
    assert!(run_rule(
        Instruction::Getfield(6),
        VType::ObjectRef(Arc::from("p/Sib")),
        &mut scratch,
    )
    .is_ok());
}

#[test]
fn invokespecial_of_an_indirect_superinterface_is_refused() {
    let mut scratch = VerifyScratch::for_class_version(69);
    let result = run_rule(
        Instruction::Invokespecial(15),
        VType::ObjectRef(Arc::from("q/Cur")),
        &mut scratch,
    );
    assert_eq!(
        rule_message(result),
        "Bad invokespecial instruction: interface method to invoke is not in a direct \
         superinterface."
    );
    assert!(scratch.take_linkage_rule_violation());
}

#[test]
fn invokespecial_of_an_unrelated_class_is_refused() {
    let mut scratch = VerifyScratch::for_class_version(69);
    let result = run_rule(
        Instruction::Invokespecial(21),
        VType::ObjectRef(Arc::from("q/Cur")),
        &mut scratch,
    );
    assert_eq!(
        rule_message(result),
        "Bad invokespecial instruction: current class isn't assignable to reference class."
    );
    let mut scratch = VerifyScratch::for_class_version(49);
    let result = run_rule(
        Instruction::Invokespecial(21),
        VType::ObjectRef(Arc::from("q/Cur")),
        &mut scratch,
    );
    assert_eq!(
        rule_message(result),
        "Illegal use of nonvirtual function call"
    );
}

#[test]
fn invokespecial_of_a_direct_superinterface_or_superclass_verifies() {
    for index in [18u16, 24] {
        let mut scratch = VerifyScratch::for_class_version(69);
        if let Err(e) = run_rule(
            Instruction::Invokespecial(index),
            VType::ObjectRef(Arc::from("q/Cur")),
            &mut scratch,
        ) {
            panic!("cp #{index} must verify: {e:?}");
        }
    }
}

#[test]
fn linkage_rule_candidates_are_decided_without_the_type_state() {
    let cp = rule_cp();
    let h = RuleHierarchy;
    let candidate = |insn: Instruction| linkage_rule_candidate(&insn, &cp, "q/Cur", &h, false);
    assert!(candidate(Instruction::Getfield(6)));
    assert!(candidate(Instruction::Putfield(6)));
    assert!(candidate(Instruction::Invokevirtual(10)));
    assert!(!candidate(Instruction::Invokevirtual(24)));
    assert!(candidate(Instruction::Invokespecial(15)));
    assert!(candidate(Instruction::Invokespecial(21)));
    assert!(!candidate(Instruction::Invokespecial(18)));
    // A hierarchy with no answer (mock / tooling) applies no rule.
    let mock = MockHierarchy;
    assert!(!linkage_rule_candidate(
        &Instruction::Invokespecial(21),
        &cp,
        "q/Cur",
        &mock,
        false
    ));
}
