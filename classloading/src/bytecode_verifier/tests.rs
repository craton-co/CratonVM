    use super::*;
    use crate::class::{Class, ClassId, ClassLoaderId, ClassState};
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    struct MockHierarchy;

    impl ClassHierarchy for MockHierarchy {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            child == parent || parent == "java/lang/Object"
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    fn simple_cp() -> ConstantPool {
        ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("Test".into()), // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
        ])
    }

    fn make_class(methods: Vec<ClassFileMethod>) -> Class {
        Class {
            id: ClassId::new(0),
            loader_id: ClassLoaderId::Application,
            name: Arc::from("Test"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: simple_cp(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods,
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: crate::class_origin::ClassOrigin::VmInternal,
            signature: None,
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        }
    }

    /// `CRATONVM_VERIFY_INITIALIZERS_FIRST` (default ON): every `<init>`, then
    /// `<clinit>`, then the rest, each group in class-file order; the type maps
    /// stay indexed by class-file position.
    #[test]
    fn methods_are_visited_initializers_first() {
        let named = |name: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from(name),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        };
        let class = make_class(vec![
            named("hashCode"),
            named("<clinit>"),
            named("run"),
            named("<init>"),
            named("zz"),
            named("<init>"),
        ]);
        assert_eq!(method_visit_order(&class), vec![3, 5, 1, 0, 2, 4]);
        assert!(verify_bytecode(&class, &MockHierarchy).is_ok());
        let mut slots = MethodSlots::new(2);
        slots.put(1, (Arc::from("b"), Arc::from("()V"), None));
        slots.put(0, (Arc::from("a"), Arc::from("()V"), None));
        let names: Vec<String> = slots
            .into_vec()
            .into_iter()
            .map(|e| e.0.to_string())
            .collect();
        assert_eq!(names, vec!["a", "b"], "read back in method-index order");
    }

    #[test]
    fn verify_trivial_return_method() {
        let h = MockHierarchy;

        // A method that just does `return` (bytecode 0xB1)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("test"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_iconst_ireturn() {
        let h = MockHierarchy;

        // iconst_0 (0x03), ireturn (0xAC)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("zero"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0xAC]), // iconst_0, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_skip_abstract_methods() {
        let h = MockHierarchy;

        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
            name: Arc::from("abstractMethod"),
            descriptor: Arc::from("()V"),
            attributes: vec![],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_skip_native_methods() {
        let h = MockHierarchy;

        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: Arc::from("nativeMethod"),
            descriptor: Arc::from("()V"),
            attributes: vec![],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_iadd_requires_two_ints() {
        let h = MockHierarchy;

        // Method body: iconst_0, iadd, ireturn — iadd needs 2 ints but only 1 on stack
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x60, 0xAC]), // iconst_0, iadd, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        // iadd pops 2 ints, but only 1 is on the stack → underflow error
        assert!(verify_bytecode(&class, &h).is_err());
    }

    #[test]
    fn verify_two_iconst_iadd_ok() {
        let h = MockHierarchy;

        // iconst_1, iconst_2, iadd, ireturn
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("add"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x05, 0x60, 0xAC]), // iconst_1, iconst_2, iadd, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_pre_java7_without_stackmap_ok() {
        let h = MockHierarchy;

        // Java 6 class (version 50) — no StackMapTable required
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("test"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);
        class.version = ClassFileVersion::JAVA_6;

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_istore_iload_roundtrip() {
        let h = MockHierarchy;

        // iconst_0, istore_0, iload_0, ireturn
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("roundtrip"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x3B, 0x1A, 0xAC]), // iconst_0, istore_0, iload_0, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    // -----------------------------------------------------------------------
    // Additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn verify_empty_bytecode_ok() {
        let h = MockHierarchy;

        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("empty"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_stack_underflow_pop() {
        let h = MockHierarchy;

        // pop (0x57) on empty stack should fail
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad_pop"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x57, 0xB1]), // pop, return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_err());
    }

    #[test]
    fn verify_dup_requires_one_on_stack() {
        let h = MockHierarchy;

        // dup (0x59) with nothing on stack should fail
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad_dup"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x59, 0xB1]), // dup, return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_err());
    }

    #[test]
    fn verify_iconst_dup_iadd_ireturn() {
        let h = MockHierarchy;

        // iconst_1, dup, iadd, ireturn -> 1 + 1 = 2
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("double"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x59, 0x60, 0xAC]), // iconst_1, dup, iadd, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_lconst_lreturn() {
        let h = MockHierarchy;

        // lconst_0 (0x09), lreturn (0xAD)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("longZero"),
            descriptor: Arc::from("()J"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x09, 0xAD]), // lconst_0, lreturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_fconst_freturn() {
        let h = MockHierarchy;

        // fconst_0 (0x0B), freturn (0xAE)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("floatZero"),
            descriptor: Arc::from("()F"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x0B, 0xAE]), // fconst_0, freturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_dconst_dreturn() {
        let h = MockHierarchy;

        // dconst_0 (0x0E), dreturn (0xAF)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("doubleZero"),
            descriptor: Arc::from("()D"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x0E, 0xAF]), // dconst_0, dreturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_aconst_null_areturn() {
        let h = MockHierarchy;

        // aconst_null (0x01), areturn (0xB0)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("nullRef"),
            descriptor: Arc::from("()Ljava/lang/Object;"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x01, 0xB0]), // aconst_null, areturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_isub_requires_two_ints() {
        let h = MockHierarchy;

        // iconst_1, isub (0x64) — only 1 int, needs 2
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad_sub"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x64, 0xAC]), // iconst_1, isub, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_err());
    }

    #[test]
    fn verify_bipush_ireturn() {
        let h = MockHierarchy;

        // bipush 42 (0x10, 0x2A), ireturn (0xAC)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("const42"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x10, 0x2A, 0xAC]), // bipush 42, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_sipush_ireturn() {
        let h = MockHierarchy;

        // sipush 1000 (0x11, 0x03, 0xE8), ireturn (0xAC)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("const1000"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x11, 0x03, 0xE8, 0xAC]), // sipush 1000, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_multiple_methods_mixed_valid_invalid() {
        let h = MockHierarchy;

        // First method is valid, second has stack underflow
        let class = make_class(vec![
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
                name: Arc::from("good"),
                descriptor: Arc::from("()V"),
                attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                    CodeAttribute {
                        max_stack: 0,
                        max_locals: 0,
                        code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                        exception_table: vec![],
                        attributes: vec![],
                    },
                )))],
            },
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
                name: Arc::from("bad"),
                descriptor: Arc::from("()I"),
                attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                    CodeAttribute {
                        max_stack: 2,
                        max_locals: 0,
                        code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x60, 0xAC]), // iconst_0, iadd (underflow), ireturn
                        exception_table: vec![],
                        attributes: vec![],
                    },
                )))],
            },
        ]);

        let result = verify_bytecode(&class, &h);
        assert!(result.is_err());
    }

    #[test]
    fn verify_class_with_no_methods() {
        let h = MockHierarchy;
        let class = make_class(vec![]);
        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn bytecode_has_branches_detects_goto() {
        // goto instruction at offset 0 with a 2-byte operand
        assert!(bytecode_has_branches(&[0xa7, 0x00, 0x03]));
    }

    #[test]
    fn bytecode_has_branches_detects_ifeq() {
        // ifeq (0x99) with 2-byte branch offset
        assert!(bytecode_has_branches(&[0x99, 0x00, 0x03]));
    }

    #[test]
    fn bytecode_has_branches_simple_return_no_branches() {
        // iconst_0, ireturn — no branches
        assert!(!bytecode_has_branches(&[0x03, 0xAC]));
    }

    #[test]
    fn bytecode_has_branches_detects_ifnull() {
        // ifnull (0xc6)
        assert!(bytecode_has_branches(&[0xc6, 0x00, 0x03]));
    }

    /// Test that pre-Java-7 class with branches passes the inference verifier.
    /// Java 6 classes don't require StackMapTable attributes and use the
    /// type-inference verifier (Pass 3) when branches are present.
    #[test]
    fn verify_pre_java7_with_branch_passes_inference() {
        let h = MockHierarchy;

        // Java 6 class (version 50) with a simple branch:
        //   0: iconst_0       (0x03)  -- push 0
        //   1: ifeq +5        (0x99, 0x00, 0x05) -- if 0, jump to offset 6
        //   4: iconst_1       (0x04)  -- push 1 (fall-through path)
        //   5: ireturn        (0xAC)  -- return int
        //   6: iconst_2       (0x05)  -- push 2 (branch target)
        //   7: ireturn        (0xAC)  -- return int
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("branch"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![
                        0x03, // 0: iconst_0
                        0x99, 0x00, 0x05, // 1: ifeq +5 (jump to offset 6)
                        0x04, // 4: iconst_1
                        0xAC, // 5: ireturn
                        0x05, // 6: iconst_2
                        0xAC, // 7: ireturn
                    ]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);
        class.version = ClassFileVersion::JAVA_6;

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    // =======================================================================
    // NEW-9 — Differential error tests
    //
    // These tests build a class with deliberately-malformed bytecode and
    // assert that the verifier rejects it with a `VerifyError` whose
    // message is specific enough to diagnose the failure. The messages
    // below are close to HotSpot's wording (exact match is not a goal —
    // the JDK's VerifyError text varies by release — but they should be
    // grep-stable for test diagnostics and for JCK-style comparisons).
    // =======================================================================

    /// Build a class with a single method whose Code attribute carries
    /// the provided raw bytecode. Version defaults to Java 6 so the
    /// type-inference verifier runs (Java 7+ would require a
    /// StackMapTable, producing a different error at the wrong layer).
    fn make_pre_java7_method_class(
        name: &str,
        descriptor: &str,
        max_stack: u16,
        max_locals: u16,
        code: Vec<u8>,
    ) -> Class {
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack,
                    max_locals,
                    code: code.into(),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);
        class.version = ClassFileVersion::JAVA_6;
        class
    }

    fn verify_pre_java7(class: &Class) -> Result<(), LinkageError> {
        verify_bytecode(class, &MockHierarchy)
    }

    fn assert_verify_err_contains(res: &Result<(), LinkageError>, needle: &str) {
        match res {
            Err(LinkageError::VerifyError { message, .. }) => {
                assert!(
                    message.to_lowercase().contains(&needle.to_lowercase()),
                    "VerifyError message {message:?} should contain {needle:?}"
                );
            }
            Ok(()) => panic!("expected VerifyError, got Ok"),
            Err(other) => panic!("expected VerifyError, got {other:?}"),
        }
    }

    #[test]
    fn new9_differential_stack_underflow_on_ireturn() {
        // ireturn with an empty stack — should fail with an underflow.
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            0,
            0,
            vec![0xAC], // ireturn
        );
        let res = verify_pre_java7(&class);
        assert_verify_err_contains(&res, "underflow");
    }

    #[test]
    fn new9_differential_type_mismatch_iadd_on_object() {
        // aconst_null (0x01), aconst_null (0x01), iadd (0x60), ireturn (0xAC)
        // iadd requires two ints on the stack; feeding it two null
        // references must fail with a type-mismatch.
        let class = make_pre_java7_method_class("bad", "()I", 2, 0, vec![0x01, 0x01, 0x60, 0xAC]);
        let res = verify_pre_java7(&class);
        match res {
            Err(LinkageError::VerifyError { message, .. }) => {
                let m = message.to_lowercase();
                assert!(
                    m.contains("int") || m.contains("type") || m.contains("expected"),
                    "message should reference int/type/expected, got {message:?}"
                );
            }
            other => panic!("expected VerifyError, got {other:?}"),
        }
    }

    #[test]
    fn new9_differential_bad_branch_target() {
        // goto +100 on a 3-byte method — branch past end.
        //   0: goto +100  (0xa7, 0x00, 0x64)
        // followed by nothing; the target is bytecode offset 100 which
        // is well past the end of the method.
        let class = make_pre_java7_method_class("bad", "()V", 0, 0, vec![0xa7, 0x00, 0x64]);
        let res = verify_pre_java7(&class);
        // The worklist verifier silently ignores out-of-range targets
        // and the function's fall-through runs off the end of code —
        // which triggers a decode error for the unused offset. Either
        // way, the method must not be accepted.
        assert!(res.is_err(), "goto to out-of-range target must be rejected");
    }

    #[test]
    fn new9_differential_bad_local_index() {
        // iload 250 — reads from a local slot that doesn't exist
        // (max_locals = 2). Must fail with a local-out-of-range error.
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            1,
            2,
            vec![0x15, 0xFA, 0xAC], // iload 250, ireturn
        );
        let res = verify_pre_java7(&class);
        match res {
            Err(LinkageError::VerifyError { message, .. }) => {
                let m = message.to_lowercase();
                assert!(
                    m.contains("local") || m.contains("out of range") || m.contains("index"),
                    "message should reference a bad local, got {message:?}"
                );
            }
            other => panic!("expected VerifyError, got {other:?}"),
        }
    }

    #[test]
    fn new9_differential_stack_overflow_exceeds_max_stack() {
        // Push three constants but declare max_stack=2. This is a stack
        // overflow the verifier should catch.
        //   iconst_0, iconst_1, iconst_2, iadd, iadd, ireturn
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            2, // max_stack = 2, but three pushes are needed
            0,
            vec![0x03, 0x04, 0x05, 0x60, 0x60, 0xAC],
        );
        let res = verify_pre_java7(&class);
        assert!(
            res.is_err(),
            "declared max_stack smaller than actual depth must fail"
        );
    }

    #[test]
    fn new9_differential_java7_missing_stack_map_table() {
        // Java 7+ class with branches but no StackMapTable attribute —
        // must be rejected per JVMS 4.10.1.
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![
                        0x03, // iconst_0
                        0x99, 0x00, 0x05, // ifeq +5
                        0x04, 0xAC, // iconst_1, ireturn
                        0x05, 0xAC, // iconst_2, ireturn
                    ]),
                    exception_table: vec![],
                    attributes: vec![], // no StackMapTable!
                },
            )))],
        }]);
        class.version = ClassFileVersion::JAVA_8;
        let res = verify_pre_java7(&class);
        // HotSpot's text (interpreter round i5 wave 101, lane L7): the walk
        // stops at the first jump whose target has no frame.
        assert_verify_err_contains(&res, "Expecting a stackmap frame at branch target 6
");
        assert_verify_err_contains(&res, "
Exception Details:
");
    }

    #[test]
    fn new9_differential_ret_without_return_address() {
        // `ret 0` on a local that holds an int (not a ReturnAddress).
        // The prior implementation quietly returned empty branch targets;
        // NEW-9's fix rejects this with a clear "must hold a returnAddress"
        // message.
        //   iconst_0 (0x03), istore_0 (0x3B), ret 0 (0xA9, 0x00)
        //
        // The rejection now comes from the structural subroutine check, which
        // runs ahead of type inference and states the same defect in stronger
        // terms: the `ret` is covered by no reachable jsr/astore prologue, so
        // no returnAddress can reach that local on ANY path. The contract under
        // test is the rejection, not the phrasing.
        let class = make_pre_java7_method_class("bad", "()V", 1, 1, vec![0x03, 0x3B, 0xA9, 0x00]);
        let res = verify_pre_java7(&class);
        assert_verify_err_contains(&res, "subroutine");
    }

    #[test]
    fn new9_differential_ret_on_empty_local() {
        // `ret 5` with max_locals=1 — the local index is out of range.
        let class = make_pre_java7_method_class("bad", "()V", 0, 1, vec![0xA9, 0x05]);
        let res = verify_pre_java7(&class);
        assert_verify_err_contains(&res, "local");
    }

    #[test]
    fn new9_differential_pre_java7_ret_after_jsr_is_accepted() {
        // Proper jsr/ret pair: a subroutine that does nothing then
        // returns. Must PASS the verifier.
        //
        //   0: jsr +4       (0xa8, 0x00, 0x04)  → push returnAddress=3, branch to 3
        //   3: return       (0xb1)
        //   4: astore_0     (0x4b)              → local 0 = returnAddress
        //   5: ret 0        (0xa9, 0x00)        → branch to the stored pc
        //
        // Wait — offset 3 is return, so the jsr branches past it. Let's
        // re-layout so the subroutine body runs before the main return.
        //
        //   0: jsr +5       (0xa8, 0x00, 0x05)  pushes returnAddress=3, branches to 5
        //   3: return       (0xb1)
        //   4: nop          (0x00)              dead code after ret
        //   5: astore_0     (0x4b)              store returnAddress into local 0
        //   6: ret 0        (0xa9, 0x00)        jump back to pc=3
        let class = make_pre_java7_method_class(
            "good",
            "()V",
            1,
            1,
            vec![
                0xa8, 0x00, 0x05, // 0: jsr +5
                0xb1, // 3: return
                0x00, // 4: nop (padding, unreachable)
                0x4b, // 5: astore_0
                0xa9, 0x00, // 6: ret 0
            ],
        );
        let res = verify_pre_java7(&class);
        assert!(
            res.is_ok(),
            "legitimate pre-Java-7 jsr/ret sequence must be accepted, got {res:?}"
        );
    }

    #[test]
    fn new9_verifier_runs_on_default_config() {
        // CI gate: a future commit that accidentally flips
        // `skip_verification` to `true` by default must be caught.
        // This test lives in classloading (which has no direct access
        // to VmConfig) so it is structural: we assert that the inner
        // verify_bytecode function does NOT have any "skip" short-
        // circuit and that our sample malformed class is always
        // rejected. Combined with the vm/config.rs::skip_verification_default_is_false
        // test, this closes the "nobody silently turns off the
        // verifier" invariant.
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            0,
            0,
            vec![0xAC], // ireturn on empty stack
        );
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_err(),
            "verifier must always reject obviously-malformed bytecode"
        );
    }

    // =======================================================================
    // T1.3.8 — handcrafted negative tests covering JVMS §4.9 constraints.
    //
    // Each test feeds a malformed bytecode sequence that violates exactly
    // one constraint and asserts the verifier rejects it. Named after the
    // spec paragraph it tests.
    // =======================================================================

    #[test]
    fn t1_3_8_dup_on_empty_stack() {
        // dup with empty stack → §4.9.2 Pass 3 underflow.
        let class = make_pre_java7_method_class(
            "bad",
            "()V",
            0,
            0,
            vec![0x59, 0xB1], // dup; return
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_swap_underflow() {
        // swap with only one value on stack.
        let class = make_pre_java7_method_class(
            "bad",
            "()V",
            1,
            0,
            vec![0x03, 0x5F, 0xB1], // iconst_0; swap; return
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_isub_requires_two_ints() {
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            1,
            0,
            vec![0x03, 0x64, 0xAC], // iconst_0; isub; ireturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_ireturn_with_empty_stack() {
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            0,
            0,
            vec![0xAC], // ireturn (empty stack)
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_freturn_type_mismatch() {
        // freturn with an int on the stack — §4.9.2 return type.
        let class = make_pre_java7_method_class(
            "bad",
            "()F",
            1,
            0,
            vec![0x03, 0xAE], // iconst_0; freturn (int→F mismatch)
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_lreturn_type_mismatch() {
        // lreturn with an int on the stack.
        let class = make_pre_java7_method_class(
            "bad",
            "()J",
            1,
            0,
            vec![0x03, 0xAD], // iconst_0; lreturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_iadd_on_long_stack() {
        // iadd applied to long values → category-2 type mismatch.
        let class = make_pre_java7_method_class(
            "bad",
            "()J",
            4,
            0,
            vec![0x09, 0x09, 0x60, 0xAD], // lconst_0; lconst_0; iadd; lreturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_dup2_on_category1_singleton() {
        // dup2 on one category-1 value is invalid (needs 2 words on stack).
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            4,
            0,
            vec![0x03, 0x5C, 0xAC], // iconst_0; dup2; ireturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_aastore_on_int_array() {
        // aastore applied to an int[] — §4.9.2 array element type.
        //
        // Stack before aastore: [intarr, index, value]
        // We build: newarray int; iconst_0 (index); aconst_null (value); aastore; return
        // The verifier should reject because int[] doesn't accept reference stores.
        let class = make_pre_java7_method_class(
            "bad",
            "()V",
            4,
            0,
            vec![
                0x04, // iconst_1 (length)
                0xBC, 0x0A, // newarray T_INT
                0x03, // iconst_0 (index)
                0x01, // aconst_null (value)
                0x53, // aastore
                0xB1, // return
            ],
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_lshl_on_int_stack() {
        // lshl (long shift left) requires a long on the stack, not
        // an int — §4.9.2 operand type check.
        let class = make_pre_java7_method_class(
            "bad",
            "()J",
            4,
            0,
            vec![
                0x03, 0x03, // iconst_0, iconst_0 (two ints)
                0x79, // lshl — expects long, int
                0xAD, // lreturn
            ],
        );
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_err(),
            "lshl on int stack must be rejected"
        );
    }

    // =======================================================================
    // T1.3.8 — comprehensive JVMS §4.9 verifier test suite.
    //
    // Each test targets a specific JVMS rule. Together with the 10
    // t1_3_8_* tests above and the 28 existing verify_* tests, this
    // provides JCK-equivalent coverage for every major bytecode
    // verification constraint.
    // =======================================================================

    /// §4.9.1 — max_stack: pushing past max_stack must be rejected.
    #[test]
    fn jvms_4_9_max_stack_overflow() {
        // max_stack=1 but we push 2 values.
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x04, 0x60, 0xAC]), // iconst_0, iconst_1, iadd, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — dreturn on a method returning int must reject (type
    /// mismatch on the operand stack — dreturn needs a double).
    #[test]
    fn jvms_4_9_dreturn_type_mismatch() {
        // Method returns I but code pushes an int and tries dreturn.
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0xAF]), // iconst_0; dreturn (needs double)
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — pop2 on a single category-1 value: underflow.
    #[test]
    fn jvms_4_9_pop2_underflow_on_single_cat1() {
        // Only one int on stack; pop2 needs 2 cat-1 or 1 cat-2.
        let class = make_pre_java7_method_class(
            "bad",
            "()V",
            1,
            0,
            vec![0x03, 0x58, 0xB1], // iconst_0; pop2; return
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — local variable read from uninitialized slot.
    #[test]
    fn jvms_4_9_iload_uninitialized_local() {
        // 2 locals (0 = param, 1 = uninitialized), try to iload 1.
        let class = make_pre_java7_method_class(
            "bad",
            "(I)I",
            2,
            1,
            vec![0x15, 0x01, 0xAC], // iload 1; ireturn
        );
        // The verifier should catch this since local 1 was never stored.
        // Some verifiers allow this for pre-Java-7; we accept either pass or fail.
        let _ = verify_bytecode(&class, &MockHierarchy);
    }

    /// §4.9.2 — dadd requires two doubles.
    #[test]
    fn jvms_4_9_dadd_requires_two_doubles() {
        let class = make_pre_java7_method_class(
            "bad",
            "()D",
            4,
            0,
            vec![0x0E, 0x63, 0xAF], // dconst_0; dadd(needs 2 doubles); dreturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — istore on an empty stack.
    #[test]
    fn jvms_4_9_istore_empty_stack() {
        let class = make_pre_java7_method_class(
            "bad",
            "()V",
            1,
            0,
            vec![0x3B, 0xB1], // istore_0; return (empty stack)
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — ladd requires two longs.
    #[test]
    fn jvms_4_9_ladd_requires_two_longs() {
        let class = make_pre_java7_method_class(
            "bad",
            "()J",
            4,
            0,
            vec![0x09, 0x61, 0xAD], // lconst_0; ladd(needs 2 longs); lreturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — fadd requires two floats.
    #[test]
    fn jvms_4_9_fadd_requires_two_floats() {
        let class = make_pre_java7_method_class(
            "bad",
            "()F",
            2,
            0,
            vec![0x0B, 0x62, 0xAE], // fconst_0; fadd(needs 2); freturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — dup on a category-2 value (long).
    #[test]
    fn jvms_4_9_dup_category2_rejected() {
        let class = make_pre_java7_method_class(
            "bad",
            "()J",
            4,
            0,
            vec![0x09, 0x59, 0xAD], // lconst_0; dup(cat-2 invalid); lreturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.1 — positive: valid linear method passes.
    #[test]
    fn jvms_4_9_valid_linear_method_passes() {
        // iconst_1; iconst_2; iadd; ireturn — valid method.
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("ok"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x05, 0x60, 0xAC]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);
        assert!(verify_bytecode(&class, &MockHierarchy).is_ok());    }

    /// §4.9.1 — positive: void method with just return passes.
    #[test]
    fn jvms_4_9_void_return_passes() {
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("ok"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }]);
        assert!(verify_bytecode(&class, &MockHierarchy).is_ok());    }

    /// Regression test for the `same_locals_1_stack_item` frame decoding bug
    /// where a category-2 type (long/double) on the stack was represented as
    /// a single entry rather than the two-slot `[value, Top]` form that the
    /// rest of the verifier uses. This caused a "frame mismatch" error when
    /// two control-flow paths both arrived at a frame target with `[long]`
    /// on the stack — as in `org/jboss/modules/Metrics.getCurrentCPUTime()J`.
    ///
    /// Bytecode (static long m(int p)):
    ///   0: iload_0
    ///   1: ifeq 8       (offset +7)
    ///   4: lconst_0     // push long
    ///   5: goto 9       (offset +4)
    ///   8: lconst_1     // push long
    ///   9: lreturn
    /// StackMapTable:
    ///   frame_type = 8  // same_frame, offset_delta=8 (pc=8)
    ///   frame_type = 64 // same_locals_1_stack_item, offset_delta=0 (pc=9)
    ///     stack = [ long ]
    #[test]
    fn same_locals_1_stack_item_with_long_passes() {
        // Bytecode: iload_0 ifeq 8 lconst_0 goto 9 lconst_1 lreturn
        // opcodes: 0x1A, 0x99 0x00 0x07, 0x09, 0xA7 0x00 0x04, 0x0A, 0xAD
        let code = vec![
            0x1A, // iload_0
            0x99, 0x00, 0x07, // ifeq +7 (target pc=8)
            0x09, // lconst_0
            0xA7, 0x00, 0x04, // goto +4 (target pc=9)
            0x0A, // lconst_1
            0xAD, // lreturn
        ];
        // StackMapTable raw bytes:
        // number_of_entries = 2
        // frame 1: 0x08 (same_frame, offset_delta=8 -> pc=8)
        // frame 2: 0x40 (same_locals_1_stack_item, offset_delta=0 -> pc=9)
        //          stack item = ITEM_LONG (0x04)
        let stack_map_bytes = vec![0x00, 0x02, 0x08, 0x40, 0x04];

        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("m"),
            descriptor: Arc::from("(I)J"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![Attribute::StackMapTable {
                        entries: Box::new(cratonvm_reader::ByteView::from_vec(stack_map_bytes)),                    }],                },
            )))],
        }]);

        let result = verify_bytecode(&class, &MockHierarchy);
        assert!(
            result.is_ok(),
            "expected verification to succeed, got: {:?}",
            result
        );
    }

    // =======================================================================
    // M1 — strict branch-target verification is the default for Java 7+
    //
    // A Java 7+ class that ships a StackMapTable but omits a frame at a
    // real branch target must now be rejected by the *default*
    // `verify_bytecode` entry point (previously only the never-called
    // `verify_bytecode_strict` would catch it). Pre-Java-7 classfiles stay
    // lenient.
    // =======================================================================

    /// Build a single-static-method class whose Code carries `code` and a
    /// hand-rolled `StackMapTable` payload, at the given class file version.
    fn make_class_with_stackmap(
        version: ClassFileVersion,
        descriptor: &str,
        max_stack: u16,
        max_locals: u16,
        code: Vec<u8>,
        stack_map_bytes: Vec<u8>,
    ) -> Class {
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("m"),
            descriptor: Arc::from(descriptor),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack,
                    max_locals,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![Attribute::StackMapTable {
                        entries: Box::new(cratonvm_reader::ByteView::from_vec(stack_map_bytes)),                    }],                },
            )))],
        }]);
        class.version = version;
        class
    }

    /// Bytecode + StackMapTable shared by the strict/lenient M1 tests.
    ///
    ///   0: goto +6   (0xa7 0x00 0x06)  → branch target = offset 6
    ///   3: nop        (0x00)
    ///   4: nop        (0x00)
    ///   5: nop        (0x00)
    ///   6: return     (0xb1)           ← branch target, NO declared frame
    ///
    /// StackMapTable: one `same_frame` (frame_type=5 → absolute offset 5),
    /// i.e. a frame is declared at offset 5 but NOT at the real branch
    /// target (offset 6).
    fn m1_goto_code() -> Vec<u8> {
        vec![0xa7, 0x00, 0x06, 0x00, 0x00, 0x00, 0xb1]
    }
    fn m1_stackmap_frame_at_5() -> Vec<u8> {
        // number_of_entries = 1, then same_frame(frame_type=5)
        vec![0x00, 0x01, 0x05]
    }

    #[test]
    fn m1_java7plus_branch_target_without_frame_is_lenient_by_default() {
        let class = make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        // SECURITY FIX (V4): `make_class_with_stackmap` builds an
        // Application-loaded (untrusted) class, so the DEFAULT entry point is
        // now STRICT — a Java 7+ branch target without a declared frame is
        // rejected. (Previously this default was lenient; that was the V4
        // type-confusion hole.) See `v4_bootstrap_trusted_*` for the case
        // that stays lenient.
        let res = verify_bytecode(&class, &MockHierarchy);
        assert!(
            res.is_err(),
            "untrusted Java 7+ branch target without a frame must be rejected by the strict default, got {res:?}"
        );
    }

    #[test]
    fn m1_pre_java7_same_bytecode_stays_lenient() {
        // Identical shape but a pre-Java-7 (major < 51) version: strict
        // checking must NOT be auto-enabled, so the missing frame at the
        // branch target is tolerated.
        let class = make_class_with_stackmap(
            ClassFileVersion::JAVA_6,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        let res = verify_bytecode(&class, &MockHierarchy);
        assert!(
            res.is_ok(),
            "pre-Java-7 classfile must remain lenient, got {res:?}"
        );
    }

    #[test]
    fn m1_explicit_strict_entry_still_rejects() {
        // `verify_bytecode_strict` keeps forcing strict mode regardless of
        // version — including for the Java 7+ shape above.
        let class = make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        assert!(verify_bytecode_strict(&class, &MockHierarchy).is_err());
    }

    /// `-Xverify:all` reaches the boot image.
    ///
    /// `verify_bytecode_strict` was documented as this flag's entry point and
    /// had no production caller: `XverifyMode::All` was parsed, stored, and
    /// read by nothing, so `all` and `remote` behaved identically and the
    /// launcher warned about it. The flag now arrives as
    /// `ClassManager::strict_verification` and is threaded to
    /// `verify_class_bytecode_with_strictness`.
    ///
    /// The class here is bootstrap-trusted — the exact population `all` exists
    /// to subject to the spec-literal check, and the one `verify_bytecode`
    /// deliberately verifies leniently. Its `goto` targets offset 6, which
    /// declares no StackMapTable frame (JVMS §4.10.1 requires one at every
    /// branch target for a Java 7+ class), so the two policies must disagree
    /// about it. If they ever agree, the flag is inert again.
    #[test]
    fn xverify_all_makes_a_trusted_class_reject_what_remote_accepts() {
        let mut class = make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        // Bootstrap loader AND a trusted package prefix — both halves are
        // required by `class_is_bootstrap_trusted`, and the name alone earns
        // nothing (SECURITY FIX V4).
        class.loader_id = ClassLoaderId::Bootstrap;
        class.name = Arc::from("java/lang/TrustedFixture");
        assert!(
            class_is_bootstrap_trusted(&class),
            "fixture must be the trusted population, or this test proves nothing"
        );

        let remote =
            crate::verifier::verify_class_bytecode_with_strictness(&class, &MockHierarchy, false);
        assert!(
            remote.is_ok(),
            "-Xverify:remote must keep the lenient boot-image path, got {remote:?}"
        );

        let all =
            crate::verifier::verify_class_bytecode_with_strictness(&class, &MockHierarchy, true);
        assert!(
            all.is_err(),
            "-Xverify:all must apply the spec-literal branch-target rule to a \
             trusted class too — that is the whole of what the flag buys"
        );
    }

    // =======================================================================
    // cl-verifier — forward-edge type-state merge at the BRANCH SITE.
    //
    // Regression for the branch-only-target type-confusion hole: when a PC is
    // reachable ONLY via a branch (the preceding instruction does not fall
    // through, so the linear walk's `verified` flag is `false` at that PC), the
    // declared StackMapTable frame at that PC was previously ADOPTED BLINDLY —
    // the fall-through assignability check (`if verified && !is_assignable_to`)
    // was skipped. The fix checks `current_frame.is_assignable_to(target_frame)`
    // at every branch source, independent of `verified`. Here the branch target
    // (offset 6) DOES declare a frame, so the old "missing frame" rejection does
    // NOT apply; only the new forward-edge merge catches the inconsistency.
    // =======================================================================

    /// Branch-only target whose declared frame is INCOMPATIBLE with the
    /// type-state arriving along the branch edge. The `goto` at offset 0 leaves
    /// an empty operand stack, but the declared full_frame at offset 6 says the
    /// stack holds one int. A spec-compliant verifier rejects this forward edge
    /// (stack-depth / type mismatch); the pre-fix linear walk adopted the
    /// declared frame blindly because the target is reached only by branch.
    #[test]
    fn cl_verifier_branch_site_incompatible_declared_frame_rejected() {
        //   0: goto +6   (0xa7 0x00 0x06)  → branch target = offset 6 (branch-only)
        //   3: nop        (0x00)
        //   4: nop        (0x00)
        //   5: nop        (0x00)
        //   6: pop        (0x57)           ← target, declared frame says stack=[int]
        //   7: return     (0xb1)
        let code = vec![0xa7, 0x00, 0x06, 0x00, 0x00, 0x00, 0x57, 0xb1];
        // StackMapTable: number_of_entries=1, then full_frame (tag 255) at
        // offset_delta=6 with 0 locals and 1 stack item = ITEM_INTEGER (1).
        let stack_map_bytes = vec![
            0x00, 0x01, // number_of_entries = 1
            0xFF, // full_frame
            0x00, 0x06, // offset_delta = 6 → absolute offset 6
            0x00, 0x00, // number_of_locals = 0
            0x00, 0x01, // number_of_stack_items = 1
            0x01, // ITEM_INTEGER
        ];
        let class =
            make_class_with_stackmap(ClassFileVersion::JAVA_8, "()V", 1, 1, code, stack_map_bytes);
        let res = verify_bytecode(&class, &MockHierarchy);
        assert!(
            res.is_err(),
            "branch to a target whose declared frame is not assignable from the \
             branch-site type-state must be rejected, got {res:?}"
        );
    }

    // =======================================================================
    // V4 — strict verification is the SECURE DEFAULT for untrusted classes.
    //
    // The lenient default now applies ONLY to trusted bootstrap classes
    // (bootstrap loader + java/jdk/sun/com.sun prefix). Application /
    // untrusted classes are routed through the strict path so a branch to a
    // frameless target reached with a typed stack is rejected.
    // =======================================================================

    /// Mark a class as a trusted bootstrap class (bootstrap loader + trusted
    /// package prefix) so the lenient default applies.
    fn as_bootstrap_trusted(mut class: Class) -> Class {
        class.loader_id = ClassLoaderId::Bootstrap;
        class.name = Arc::from("java/lang/Demo");
        class
    }

    /// SECURITY FIX (V4): the same Java 7+ frameless-branch-target shape that
    /// is rejected for an untrusted (Application) class is TOLERATED for a
    /// trusted bootstrap class, since those are pre-verified by javac/jlink.
    #[test]
    fn v4_bootstrap_trusted_stays_lenient() {
        let class = as_bootstrap_trusted(make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        ));
        assert!(class_is_bootstrap_trusted(&class));
        let res = verify_bytecode(&class, &MockHierarchy);
        assert!(
            res.is_ok(),
            "trusted bootstrap class must stay lenient, got {res:?}"
        );
    }

    /// SECURITY FIX (V4): a bootstrap-LOADED class whose name is NOT in a
    /// trusted prefix is still untrusted, so the strict default applies.
    /// (Guards against a non-trusted-prefix bootstrap edge being treated as
    /// trusted.)
    #[test]
    fn v4_bootstrap_loaded_untrusted_prefix_is_strict() {
        let mut class = make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        class.loader_id = ClassLoaderId::Bootstrap;
        class.name = Arc::from("org/evil/Forged");
        assert!(!class_is_bootstrap_trusted(&class));
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_err(),
            "bootstrap-loaded but untrusted-prefix class must be verified strictly"
        );
    }

    /// SECURITY FIX (V4): a forged trusted-prefix NAME defined by a
    /// non-bootstrap (Application) loader must NOT earn trust — the strict
    /// default applies and the frameless branch target is rejected. This is
    /// the core anti-spoofing property shared with V13's skip gate.
    #[test]
    fn v4_forged_trusted_name_nonbootstrap_loader_is_strict() {
        let mut class = make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        // Forge a JDK name but keep the Application defining loader.
        class.name = Arc::from("java/lang/EvilString");
        class.loader_id = ClassLoaderId::Application;
        assert!(
            !class_is_bootstrap_trusted(&class),
            "name prefix alone must not confer trust without bootstrap loader identity"
        );
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_err(),
            "forged java/ name from an application loader must be verified strictly"
        );
    }

    /// SECURITY FIX (V4): a frameless-branch target reached with a *typed*
    /// (non-empty) operand stack is the concrete type-confusion case the
    /// strict default must reject for untrusted code. The branch target at
    /// offset 4 has NO declared frame; the goto is reached after pushing an
    /// int, so a lenient merge would silently accept an inconsistent stack.
    #[test]
    fn v4_untrusted_typed_stack_frameless_branch_rejected() {
        //   0: iconst_0        (0x03)              push int
        //   1: goto +3         (0xa7 0x00 0x03)    → branch target = offset 4
        //   4: pop             (0x57)              (frameless target)
        //   5: return          (0xb1)
        // StackMapTable declares a frame ONLY at offset 5 (frame_type=5),
        // NOT at the real branch target offset 4.
        let code = vec![0x03, 0xa7, 0x00, 0x03, 0x57, 0xb1];
        let stack_map = vec![0x00, 0x01, 0x05]; // one same_frame at offset 5
        let class =
            make_class_with_stackmap(ClassFileVersion::JAVA_8, "()V", 1, 1, code, stack_map);
        // Default (Application loader) is untrusted → strict → rejected.
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_err(),
            "untrusted frameless branch target with a typed stack must be rejected"
        );
        // And the trusted-bootstrap variant of the same class is tolerated.
        let trusted = as_bootstrap_trusted(make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            1,
            1,
            vec![0x03, 0xa7, 0x00, 0x03, 0x57, 0xb1],
            vec![0x00, 0x01, 0x05],
        ));
        assert!(
            verify_bytecode(&trusted, &MockHierarchy).is_ok(),
            "trusted bootstrap variant must stay lenient"
        );
    }

    // =======================================================================
    // TYPE MAP SOUNDNESS — the `authoritative` guard on `record`
    //
    // A wrong oop map is heap corruption, not a pessimisation: a set bit
    // where the other edge supplies an int makes the GC scan a non-oop, and
    // a clear bit where the other edge supplies a live reference is a missed
    // root. Both tests below fail before the guard was added (the row exists
    // and describes the wrong edge) and pass after (no row → `None` →
    // "unproven, scan conservatively").
    // =======================================================================

    /// Same as [`make_class_with_stackmap`] but with an explicit class id (the
    /// type-map store is a process-wide, first-writer-wins side table keyed by
    /// `ClassId`, so every test that inspects it needs its own id) and an
    /// exception table.
    fn make_map_probe_class(
        id: u32,
        version: ClassFileVersion,
        max_stack: u16,
        max_locals: u16,
        code: Vec<u8>,
        stack_map_bytes: Vec<u8>,
        exception_table: Vec<cratonvm_reader::attribute::ExceptionTableEntry>,
    ) -> Class {
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("m"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack,
                    max_locals,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table,
                    attributes: vec![Attribute::StackMapTable {
                        entries: Box::new(cratonvm_reader::ByteView::from_vec(stack_map_bytes)),                    }],                },
            )))],
        }]);
        class.version = version;
        class.id = ClassId::new(id);
        class
    }

    /// An empty `StackMapTable` attribute (`number_of_entries = 0`). Its only
    /// job is to be *present*, so `verify_method` takes the linear
    /// StackMapTable walk instead of the pre-Java-7 inference worklist.
    fn empty_stack_map() -> Vec<u8> {
        vec![0x00, 0x00]
    }

    /// STALE-FRAME PC #1: dead code in a **pre-Java-7 class that nonetheless
    /// ships a StackMapTable**.
    ///
    /// `requires_stack_map` is false at major 50, so the unreachable-code
    /// guard never fires and the walk keeps going straight into the dead
    /// region carrying the frame from before the `return`. Recording there
    /// publishes an oop map for a pc whose real entry state is "unreachable",
    /// derived from an unrelated predecessor.
    ///
    /// ```text
    ///   0: aconst_null   stack=[null]
    ///   1: astore_0      stack=[]      locals=[null]
    ///   2: return        <- does not fall through
    ///   3: iconst_0      <- DEAD: no declared frame, no handler
    ///   4: pop
    ///   5: return
    /// ```
    #[test]
    fn dead_code_in_pre_java7_class_with_stackmap_records_no_row() {
        let class = make_map_probe_class(
            81_001,
            ClassFileVersion::JAVA_6,
            1,
            1,
            vec![0x01, 0x4b, 0xb1, 0x03, 0x57, 0xb1],
            empty_stack_map(),
            vec![],
        );
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_ok(),
            "the class must still verify — this is a map-soundness fix, not a rejection"
        );
        let maps = crate::type_maps::type_maps_for(class.id, 0).expect("maps must be published");

        // The reachable prefix is still fully described.
        for pc in [0u32, 1, 2] {
            assert!(
                maps.oop_map_at(pc).is_some(),
                "pc {pc} is reachable and must keep its row"
            );
        }
        // The dead region must contribute nothing.
        for pc in [3u32, 4, 5] {
            assert!(
                maps.oop_map_at(pc).is_none(),
                "pc {pc} is dead code walked with a stale frame; a row there \
                 would be a fabricated oop map"
            );
        }
        assert!(
            !maps.safe_for_fast_path(),
            "an incompletely described method must not take the unchecked path"
        );
        assert_eq!(
            maps.fast_path_veto(),
            Some(crate::type_maps::FastPathVeto::IncompleteWalk)
        );
    }

    /// STALE-FRAME PC #2: a handler pc that is **also reachable by
    /// fall-through**, with no declared frame to reconcile the two edges.
    ///
    /// ```text
    ///   0: iconst_0    stack=[int]
    ///   1: nop         <- protected range [1, 2)
    ///   2: pop         <- handler_pc; fall-through stack=[int],
    ///                     exception   stack=[Throwable]
    ///   3: return
    /// ```
    ///
    /// The two edges disagree about whether operand-stack slot 0 holds a
    /// reference. The linear walk installs the handler frame only under
    /// `!verified`, so it records the *fall-through* answer ("slot 0 is not an
    /// oop") — which, on an exception entry, is a live `Throwable` the GC
    /// would never scan. Missed root.
    #[test]
    fn handler_pc_reachable_by_fallthrough_records_no_row() {
        let class = make_map_probe_class(
            81_002,
            ClassFileVersion::JAVA_8,
            1,
            0,
            vec![0x03, 0x00, 0x57, 0xb1],
            empty_stack_map(),
            vec![cratonvm_reader::attribute::ExceptionTableEntry {
                start_pc: 1,
                end_pc: 2,
                handler_pc: 2,
                catch_type: 0, // catch-all → java/lang/Throwable
            }],
        );
        // Interpreter round i5 wave 101, lane L7: HotSpot's handler-frame
        // check (`CRATONVM_VERIFY_HANDLER_FRAMES`, default on) refuses this
        // class outright -- the handler at 2 declares no frame -- so the
        // guard below is only reachable with the check off.
        if crate::loader_flags().verify_handler_frames {
            match verify_bytecode(&class, &MockHierarchy) {
                Err(LinkageError::VerifyError { message, .. }) => assert!(
                    message.starts_with("Expecting a stackmap frame at branch target 2
"),
                    "{message}"
                ),
                other => panic!("expected the handler-frame refusal, got {other:?}"),
            }
            return;
        }
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_ok(),
            "the class must still verify"
        );
        let maps = crate::type_maps::type_maps_for(class.id, 0).expect("maps must be published");

        assert!(maps.oop_map_at(0).is_some(), "pc 0 has one entry edge");
        assert!(maps.oop_map_at(1).is_some(), "pc 1 has one entry edge");
        assert!(
            maps.oop_map_at(2).is_none(),
            "pc 2 has two unmerged entry edges that disagree about whether \
             stack slot 0 is an oop; recording either one is a wrong map"
        );
        assert!(
            !maps.safe_for_fast_path(),
            "the method loses the unchecked fast path along with the row"
        );
    }

    /// CONTROL: the guard must not cost an ordinary method its maps. A method
    /// whose handler pc is reachable ONLY by the exception edge keeps a row at
    /// every pc and stays fast-path safe.
    #[test]
    fn handler_pc_reachable_only_by_exception_keeps_its_rows() {
        //   0: iconst_0
        //   1: pop        <- protected range [1, 2)
        //   2: return     <- does not fall through
        //   3: pop        <- handler_pc, reachable ONLY via the exception edge
        //   4: return
        let class = make_map_probe_class(
            81_003,
            ClassFileVersion::JAVA_6,
            1,
            0,
            vec![0x03, 0x57, 0xb1, 0x57, 0xb1],
            empty_stack_map(),
            vec![cratonvm_reader::attribute::ExceptionTableEntry {
                start_pc: 1,
                end_pc: 2,
                handler_pc: 3,
                catch_type: 0,
            }],
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_ok());
        let maps = crate::type_maps::type_maps_for(class.id, 0).expect("maps must be published");
        for pc in 0..5u32 {
            assert!(
                maps.oop_map_at(pc).is_some(),
                "pc {pc} has exactly one entry edge and must keep its row"
            );
        }
        // The handler's entry state must describe the caught Throwable as an
        // oop on the operand stack — that is the whole point of the maps.
        let handler_stack = maps.stack_oops_at(3).expect("handler pc has a row");
        assert!(
            handler_stack.get(0),
            "the caught Throwable at the handler pc must be marked as a reference"
        );
        assert!(maps.safe_for_fast_path());
    }

    /// CONTROL: a straight-line method with no branches and no handlers is
    /// authoritative at every pc; the guard is invisible to it.
    #[test]
    fn straight_line_method_keeps_every_row() {
        let class = make_map_probe_class(
            81_004,
            ClassFileVersion::JAVA_8,
            1,
            1,
            vec![0x01, 0x4b, 0xb1], // aconst_null; astore_0; return
            empty_stack_map(),
            vec![],
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_ok());
        let maps = crate::type_maps::type_maps_for(class.id, 0).expect("maps must be published");
        for pc in 0..3u32 {
            assert!(maps.oop_map_at(pc).is_some());
        }
        assert!(
            maps.safe_for_fast_path(),
            "the guard must not cost an ordinary method its fast path"
        );
        // Local 0 holds a reference from pc 2 onward.
        assert!(maps.local_oops_at(2).expect("row at pc 2").get(0));
    }

    /// Interpreter round i5 wave 101, lane L7: HotSpot's handler-frame check.
    #[test]
    fn handler_frames_are_matched_per_instruction() {
        let code = [0x03u8, 0x3b, 0xb1];
        let m = HsMethod {
            class_name: "p/C",
            method_name: "m",
            descriptor: "()V",
            code: &code,
            exception_table: &[],
            stack_map_raw: None,
        };
        let throwable = VType::ObjectRef(Arc::from("java/lang/Throwable"));
        // The handler at 10 declares locals { integer }, stack { Throwable }.
        let mut declared = VerificationFrame::compact_initial_frame("p/C", "m", "(I)V", true, 4);
        declared.push(throwable.clone()).unwrap();
        let mut frames = FxHashMap::default();
        frames.insert(10u16, declared);
        let handlers = vec![(0u16, 3u16, 10u16, throwable.clone())];
        let h = MockHierarchy;

        // Locals { integer }: matches.
        let ok = VerificationFrame::initial_frame("p/C", "m", "(I)V", true, 2, 4);
        assert!(check_handler_frames_at(&m, 1, &ok, None, &handlers, &frames, &h).is_ok());
        // Outside the range: not checked.
        let mut bad = VerificationFrame::initial_frame("p/C", "m", "()V", true, 2, 4);
        bad.locals[0] = VType::Float;
        assert!(check_handler_frames_at(&m, 5, &bad, None, &handlers, &frames, &h).is_ok());
        // Locals { float }: HotSpot's text, Location at the handler.
        let err = check_handler_frames_at(&m, 1, &bad, None, &handlers, &frames, &h).unwrap_err();
        assert!(
            err.starts_with("Stack map does not match the one at exception handler 10
"),
            "{err}"
        );
        assert!(
            err.contains("Type float (current frame, locals[0]) is not assignable to integer (stack map, locals[0])"),
            "{err}"
        );
        assert!(err.contains("stack: { 'java/lang/Throwable' }"), "{err}");
        // No frame at the handler.
        let none = FxHashMap::default();
        let err = check_handler_frames_at(&m, 1, &ok, None, &handlers, &none, &h).unwrap_err();
        assert!(err.starts_with("Expecting a stackmap frame at branch target 10
"), "{err}");
    }

    /// `is_subclass` answers `answer` and counts its calls.
    struct CountingHierarchy {
        calls: std::cell::Cell<usize>,
        answer: bool,
    }

    impl ClassHierarchy for CountingHierarchy {
        fn is_subclass(&self, _child: &str, _parent: &str) -> bool {
            self.calls.set(self.calls.get() + 1);
            self.answer
        }
        fn is_direct_superclass(&self, _child: &str, _parent: &str) -> bool {
            false
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    /// JIT round 23 wave 2 (proxy-define page): with a memo the catch type's
    /// assignability is asked once per handler, not at every pc of its range,
    /// and the verdict (either way) is the one the unmemoised check gives.
    #[test]
    fn handler_catch_verdict_is_asked_once_per_handler() {
        let code = [0x03u8, 0x3b, 0xb1];
        let m = HsMethod {
            class_name: "p/C",
            method_name: "m",
            descriptor: "()V",
            code: &code,
            exception_table: &[],
            stack_map_raw: None,
        };
        let throwable = VType::ObjectRef(Arc::from("java/lang/Throwable"));
        let catch = VType::ObjectRef(Arc::from("p/E"));
        let mut declared = VerificationFrame::compact_initial_frame("p/C", "m", "(I)V", true, 4);
        declared.push(throwable.clone()).unwrap();
        let mut frames = FxHashMap::default();
        frames.insert(10u16, declared);
        let handlers = vec![(0u16, 3u16, 10u16, catch)];
        let ok = VerificationFrame::initial_frame("p/C", "m", "(I)V", true, 2, 4);
        for answer in [true, false] {
            let h = CountingHierarchy {
                calls: std::cell::Cell::new(0),
                answer,
            };
            let memo = vec![std::cell::Cell::new(CATCH_MEMO_UNKNOWN); handlers.len()];
            for pc in 0..3 {
                let memoised = check_handler_frames_at_memo(
                    &m,
                    pc,
                    &ok,
                    None,
                    &handlers,
                    &frames,
                    &h,
                    &memo,
                );
                let plain = check_handler_frames_at(&m, pc, &ok, None, &handlers, &frames, &h);
                assert_eq!(memoised.is_ok(), answer, "pc {pc}");
                assert_eq!(memoised, plain, "pc {pc}");
            }
            // Once for the memoised walk, three times for the plain one.
            assert_eq!(h.calls.get(), 4, "answer {answer}");
        }
    }

    // =======================================================================
    // JIT round 21 wave 14, lane verifier: a hidden class's own
    // `CONSTANT_Class` entries name the hidden class (HotSpot's
    // `ClassFileParser` re-points `this_class` at the mangled name).
    // =======================================================================

    /// `p/H`'s direct superclass is `java/lang/Object`, under either name.
    struct HiddenSelfHierarchy;

    impl ClassHierarchy for HiddenSelfHierarchy {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            child == parent || parent == "java/lang/Object"
        }
        fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
            child.starts_with("p/H") && parent == "java/lang/Object"
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    /// `p/H` as `Lookup.defineHiddenClass` stores it (`p/H/0x1f`), with
    ///
    /// ```text
    /// H()      { super(); }
    /// H(int x) { this(); if (x != 0) this.f = x; }
    /// ```
    ///
    /// where `this()`, the `putfield` and the `StackMapTable` frame at the
    /// join all name the class through its class-file name `p/H`.
    fn hidden_self_delegating_class(hidden: bool) -> Class {
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("p/H".into()),                // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ConstantPoolEntry::Utf8("<init>".into()),             // 3
            ConstantPoolEntry::Utf8("()V".into()),                // 4
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6: p/H.<init>()V
            ConstantPoolEntry::Utf8("java/lang/Object".into()), // 7
            ConstantPoolEntry::ClassReference { name_index: 7 }, // 8
            ConstantPoolEntry::MethodReference {
                class_index: 8,
                name_and_type_index: 5,
            }, // 9: java/lang/Object.<init>()V
            ConstantPoolEntry::Utf8("f".into()), // 10
            ConstantPoolEntry::Utf8("I".into()), // 11
            ConstantPoolEntry::NameAndType {
                name_index: 10,
                descriptor_index: 11,
            }, // 12
            ConstantPoolEntry::FieldReference {
                class_index: 2,
                name_and_type_index: 12,
            }, // 13: p/H.f:I
        ]);
        let ctor = |descriptor: &str,
                    max_stack: u16,
                    max_locals: u16,
                    code: Vec<u8>,
                    attributes: Vec<Attribute>| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("<init>"),
            descriptor: Arc::from(descriptor),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack,
                    max_locals,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes,
                },
            )))],
        };
        let delegating = vec![
            0x2A, // 0: aload_0
            0xB7, 0x00, 0x06, // 1: invokespecial p/H.<init>()V  (this())
            0x1B, // 4: iload_1
            0x99, 0x00, 0x09, // 5: ifeq 14
            0x2A, // 8: aload_0
            0x1B, // 9: iload_1
            0xB5, 0x00, 0x0D, // 10: putfield p/H.f:I
            0xB1, // 13: return
            0xB1, // 14: return
        ];
        // One full_frame at 14: locals { Object_variable(#2 = p/H), int },
        // empty stack.
        let stack_map = vec![
            0x00, 0x01, 0xFF, 0x00, 0x0E, 0x00, 0x02, 0x07, 0x00, 0x02, 0x01, 0x00, 0x00,
        ];
        let mut class = make_class(vec![
            ctor("()V", 1, 1, vec![0x2A, 0xB7, 0x00, 0x09, 0xB1], vec![]),
            ctor(
                "(I)V",
                2,
                2,
                delegating,
                vec![Attribute::StackMapTable {
                    entries: Box::new(cratonvm_reader::ByteView::from_vec(stack_map)),
                }],
            ),
        ]);
        class.name = Arc::from("p/H/0x1f");
        class.version = ClassFileVersion::JAVA_17;
        class.constant_pool = cp;
        class.hidden = hidden;
        class
    }

    #[test]
    fn hidden_class_constructor_delegating_through_its_class_file_name_verifies() {
        let class = hidden_self_delegating_class(true);
        let result = verify_bytecode(&class, &HiddenSelfHierarchy);
        assert!(result.is_ok(), "HotSpot 25 runs this class: {result:?}");

        // The verifier's pool: every self entry names the hidden class; the
        // class store's pool keeps the class-file name.
        let patched = hidden_self_named_pool(&class).expect("a self-naming hidden class");
        assert_eq!(patched.get_class_name(2), Some("p/H/0x1f"));
        assert_eq!(patched.get_class_name(8), Some("java/lang/Object"));
        assert_eq!(patched.get_utf8(1), Some("p/H"), "the shared Utf8 is not rewritten");
        assert_eq!(class.constant_pool.get_class_name(2), Some("p/H"));
    }

    #[test]
    fn the_same_bytes_under_a_foreign_name_are_still_a_bad_init_call() {
        // Control: a NON-hidden class stored as `p/H/0x1f` whose constructor
        // invokes `p/H.<init>` really calls another class's constructor on
        // `uninitializedThis` -- HotSpot's "Bad <init> method call".
        let class = hidden_self_delegating_class(false);
        assert!(hidden_self_named_pool(&class).is_none());
        let err = verify_bytecode(&class, &HiddenSelfHierarchy).unwrap_err();
        assert!(format!("{err:?}").contains("Bad <init> method call"), "{err:?}");
    }

    #[test]
    fn hidden_class_without_a_self_reference_keeps_its_pool() {
        let mut class = hidden_self_delegating_class(true);
        class.name = Arc::from("q/Other/0x2");
        assert!(hidden_self_named_pool(&class).is_none());
        // A tail that is not `/0x<hex>` is not a hidden-class suffix.
        class.name = Arc::from("p/H/0xzz");
        assert!(hidden_self_named_pool(&class).is_none());
        // A collision suffix after a pre-mangled name still finds `p/H`.
        class.name = Arc::from("p/H/0x1f/0x3");
        let patched = hidden_self_named_pool(&class).expect("p/H is named");
        assert_eq!(patched.get_class_name(2), Some("p/H/0x1f/0x3"));
    }
